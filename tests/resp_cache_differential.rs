//! Optional real-server oracle for the supported same-slot RESP2 subset.
#![cfg(unix)]
use nulang::runtime::{cache::CacheStore, resp_cache::execute_frame};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct Oracle {
    child: Child,
    dir: PathBuf,
}
impl Drop for Oracle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
impl Oracle {
    fn start() -> (Self, BufReader<UnixStream>) {
        let exe = std::env::var_os("NULANG_RESP_ORACLE")
            .expect("set NULANG_RESP_ORACLE to redis-server or valkey-server");
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("nl-resp-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let socket = dir.join("oracle.sock");
        let child = Command::new(exe)
            .args(["--port", "0", "--unixsocket"])
            .arg(&socket)
            .args([
                "--unixsocketperm",
                "700",
                "--save",
                "",
                "--appendonly",
                "no",
                "--daemonize",
                "no",
            ])
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|error| {
                let _ = std::fs::remove_dir_all(&dir);
                panic!("cannot start RESP oracle: {error}");
            });
        let mut oracle = Self { child, dir };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(stream) = UnixStream::connect(&socket) {
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                return (oracle, BufReader::new(stream));
            }
            assert!(
                oracle.child.try_wait().unwrap().is_none(),
                "oracle exited before readiness"
            );
            assert!(Instant::now() < deadline, "oracle startup timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
fn frame(args: &[&[u8]]) -> Vec<u8> {
    let mut wire = format!("*{}\r\n", args.len()).into_bytes();
    for arg in args {
        wire.extend_from_slice(format!("{}{}\r\n", '$', arg.len()).as_bytes());
        wire.extend_from_slice(arg);
        wire.extend_from_slice(b"\r\n");
    }
    wire
}
fn reply(reader: &mut impl BufRead) -> Vec<u8> {
    let mut header = Vec::new();
    assert!(
        reader.read_until(b'\n', &mut header).unwrap() > 0,
        "oracle closed"
    );
    assert!(header.ends_with(b"\r\n"));
    match header[0] {
        b'+' | b'-' | b':' => {}
        kind @ (b'$' | b'*') => {
            let len: i64 = std::str::from_utf8(&header[1..header.len() - 2])
                .unwrap()
                .parse()
                .unwrap();
            assert!((-1..=1_048_576).contains(&len));
            if len >= 0 && kind == b'$' {
                let start = header.len();
                header.resize(start + len as usize + 2, 0);
                reader.read_exact(&mut header[start..]).unwrap();
                assert!(header.ends_with(b"\r\n"));
            } else if len >= 0 {
                for _ in 0..len {
                    header.extend(reply(reader));
                }
            }
        }
        _ => panic!("unexpected reply: {header:?}"),
    }
    header
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
fn reference(oracle: &mut BufReader<UnixStream>, args: &[&[u8]]) -> Vec<u8> {
    oracle.get_mut().write_all(&frame(args)).unwrap();
    reply(oracle)
}
#[test]
#[ignore = "requires an explicitly selected local Redis/Valkey executable"]
fn supported_commands_match_real_server() {
    let (_process, mut oracle) = Oracle::start();
    let mut store = CacheStore::new();
    let binary = b"\x00\xff\r\n\x80";
    let commands: &[&[&[u8]]] = &[
        &[b"PING"],
        &[b"pInG", binary],
        &[b"GET", b"missing{t}"],
        &[b"SET", b"\x00{t}", binary],
        &[b"GET", b"\x00{t}"],
        &[b"MSET", b"a{t}", b"41", b"b{t}", b""],
        &[b"MGET", b"a{t}", b"b{t}", b"missing{t}"],
        &[b"EXISTS", b"a{t}", b"a{t}", b"missing{t}"],
        &[b"INCR", b"a{t}"],
        &[b"GET", b"a{t}"],
        &[b"TTL", b"a{t}"],
        &[b"EXPIRE", b"missing{t}", b"60"],
        &[b"EXPIRE", b"a{t}", b"0"],
        &[b"TTL", b"a{t}"],
        &[b"SET", b"a{t}", b"1", b"EX", b"60"],
        &[b"SET", b"a{t}", b"2"],
        &[b"TTL", b"a{t}"],
        &[b"DEL", b"a{t}", b"b{t}", b"a{t}"],
        &[b"GET", b"a{t}"],
    ];
    for (index, args) in commands.iter().enumerate() {
        assert_eq!(
            candidate(&mut store, args, 0),
            reference(&mut oracle, args),
            "command index {index}: {args:?}"
        );
    }
}
#[test]
#[ignore = "requires an explicitly selected local Redis/Valkey executable"]
fn increment_rejects_noncanonical_numbers_without_mutation() {
    let (_process, mut oracle) = Oracle::start();
    let mut store = CacheStore::new();
    for raw in [
        b"+1".as_slice(),
        b"01",
        b"-0",
        b"-01",
        b" 1",
        b"9223372036854775807",
        b"-9223372036854775808",
        b"0",
        b"-1",
    ] {
        for args in [
            &[b"SET".as_slice(), b"counter", raw][..],
            &[b"INCR".as_slice(), b"counter"][..],
            &[b"GET".as_slice(), b"counter"][..],
        ] {
            assert_eq!(
                candidate(&mut store, args, 0),
                reference(&mut oracle, args),
                "value {raw:?}, command {args:?}"
            );
        }
    }
}
#[test]
#[ignore = "requires an explicitly selected local Redis/Valkey executable"]
fn ttl_rounds_to_nearest_second_like_reference() {
    let (_process, mut oracle) = Oracle::start();
    let mut store = CacheStore::new();
    // Reference-only Lua makes SET and TTL share the server's command-time snapshot.
    let expected = reference(
        &mut oracle,
        &[
            b"EVAL",
            b"redis.call('SET', KEYS[1], 'v', 'PX', 1900); return redis.call('TTL', KEYS[1])",
            b"1",
            b"k",
        ],
    );
    assert_eq!(expected, b":2\r\n");
    assert_eq!(
        candidate(&mut store, &[b"SET", b"k", b"v", b"PX", b"1900"], 0),
        b"+OK\r\n"
    );
    assert_eq!(candidate(&mut store, &[b"TTL", b"k"], 0), expected);
    assert_eq!(candidate(&mut store, &[b"TTL", b"k"], 401), b":1\r\n");
    assert_eq!(candidate(&mut store, &[b"TTL", b"k"], 1900), b":-2\r\n");
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

#[test]
#[ignore = "requires an explicitly selected local Redis/Valkey executable"]
fn seeded_same_slot_mutation_sequences_match_reference() {
    let (_process, mut oracle) = Oracle::start();
    let mut store = CacheStore::new();
    let keys = [b"a{seed}".as_slice(), b"b{seed}", b"c{seed}", b"d{seed}"];
    let values = [
        b"0".as_slice(),
        b"-1",
        b"01",
        b"+1",
        b"\x00\xff\r\n",
        b"",
        b"9223372036854775807",
    ];
    for seed in [1u64, 42, 0xdeadbeef] {
        let mut state = seed;
        for step in 0..256 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let key = keys[(state >> 8) as usize % keys.len()];
            let other = keys[(state >> 16) as usize % keys.len()];
            let value = values[(state >> 24) as usize % values.len()];
            let args: &[&[u8]] = match state % 8 {
                0 => &[b"SET", key, value],
                1 => &[b"GET", key],
                2 => &[b"INCR", key],
                3 => &[b"DEL", key, other],
                4 => &[b"EXISTS", key, other],
                5 => &[b"MSET", key, value, other, b"42"],
                6 => &[b"MGET", key, other],
                _ => &[b"TTL", key],
            };
            assert_eq!(
                candidate(&mut store, args, 0),
                reference(&mut oracle, args),
                "seed {seed}, step {step}, args {args:?}"
            );
        }
    }
}
