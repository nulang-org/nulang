//! RESP command execution over the shard-local cache kernel.
//!
//! This layer owns Redis protocol semantics. It deliberately invokes
//! `CacheStore` directly and never sends an actor message for a local command.
//! The caller is responsible for routing the command to the physical owner of
//! its Redis logical slot before invoking this module.

use super::cache::{redis_slot, CacheIncrementError, CacheStore, CacheTtl, CacheValueView};
use super::resp::{
    parse_command, write_array_len, write_bulk, write_bulk_integer, write_error, write_integer,
    write_null_bulk, write_simple, RespArgs, RespCommand, RespParseError,
};

const ERR_INTEGER: &[u8] = b"ERR value is not an integer or out of range";
const ERR_SYNTAX: &[u8] = b"ERR syntax error";
const ERR_SET_EXPIRE: &[u8] = b"ERR invalid expire time in 'set' command";
const ERR_CROSS_SLOT: &[u8] = b"CROSSSLOT Keys in request don't hash to the same slot";
const ERR_UNKNOWN: &[u8] = b"ERR unknown command";

/// Parse and execute exactly one RESP command frame.
///
/// Returns the number of bytes consumed from `input`. A pipelined caller can
/// remove that prefix and immediately invoke this function again. `Ok(None)`
/// means the frame is incomplete and more socket bytes are required.
pub fn execute_frame(
    store: &mut CacheStore,
    input: &[u8],
    now_ms: u64,
    out: &mut Vec<u8>,
) -> Result<Option<usize>, RespParseError> {
    let Some((command, consumed)) = parse_command(input)? else {
        return Ok(None);
    };
    execute_command(store, command, now_ms, out);
    Ok(Some(consumed))
}

/// Execute a validated RESP command against one shard-local cache store.
///
/// Multi-key commands fail with Redis-compatible `CROSSSLOT` before any
/// mutation when their keys do not share the same logical slot.
pub fn execute_command(
    store: &mut CacheStore,
    command: RespCommand<'_>,
    now_ms: u64,
    out: &mut Vec<u8>,
) {
    let name = command.name();

    if name.eq_ignore_ascii_case(b"PING") {
        execute_ping(command, out);
    } else if name.eq_ignore_ascii_case(b"GET") {
        execute_get(store, command, now_ms, out);
    } else if name.eq_ignore_ascii_case(b"SET") {
        execute_set(store, command, now_ms, out);
    } else if name.eq_ignore_ascii_case(b"DEL") {
        execute_del(store, command, now_ms, out);
    } else if name.eq_ignore_ascii_case(b"EXISTS") {
        execute_exists(store, command, now_ms, out);
    } else if name.eq_ignore_ascii_case(b"INCR") {
        execute_incr(store, command, now_ms, out);
    } else if name.eq_ignore_ascii_case(b"EXPIRE") {
        execute_expire(store, command, now_ms, out);
    } else if name.eq_ignore_ascii_case(b"TTL") {
        execute_ttl(store, command, now_ms, out);
    } else if name.eq_ignore_ascii_case(b"MGET") {
        execute_mget(store, command, now_ms, out);
    } else if name.eq_ignore_ascii_case(b"MSET") {
        execute_mset(store, command, now_ms, out);
    } else {
        write_error(out, ERR_UNKNOWN);
    }
}

fn execute_ping(command: RespCommand<'_>, out: &mut Vec<u8>) {
    match command.argc() {
        0 => write_simple(out, b"PONG"),
        1 => write_bulk(out, command.args().next().expect("validated argument")),
        _ => wrong_arity(out, b"ping"),
    }
}

fn execute_get(
    store: &mut CacheStore,
    command: RespCommand<'_>,
    now_ms: u64,
    out: &mut Vec<u8>,
) {
    if command.argc() != 1 {
        wrong_arity(out, b"get");
        return;
    }

    let key = command.args().next().expect("validated argument");
    write_value(store.get(key, now_ms), out);
}

fn execute_set(
    store: &mut CacheStore,
    command: RespCommand<'_>,
    now_ms: u64,
    out: &mut Vec<u8>,
) {
    if command.argc() != 2 && command.argc() != 4 {
        wrong_arity(out, b"set");
        return;
    }

    let mut args = command.args();
    let key = args.next().expect("validated key");
    let value = args.next().expect("validated value");
    let ttl_ms = if command.argc() == 4 {
        let option = args.next().expect("validated option");
        let raw_ttl = args.next().expect("validated ttl");
        let Some(amount) = parse_i64(raw_ttl) else {
            write_error(out, ERR_INTEGER);
            return;
        };
        if amount <= 0 {
            write_error(out, ERR_SET_EXPIRE);
            return;
        }

        if option.eq_ignore_ascii_case(b"PX") {
            Some(amount as u64)
        } else if option.eq_ignore_ascii_case(b"EX") {
            let Some(ms) = (amount as u64).checked_mul(1_000) else {
                write_error(out, ERR_SET_EXPIRE);
                return;
            };
            Some(ms)
        } else {
            write_error(out, ERR_SYNTAX);
            return;
        }
    } else {
        None
    };

    store.set_bytes(key, value, ttl_ms, now_ms);
    write_simple(out, b"OK");
}

fn execute_del(
    store: &mut CacheStore,
    command: RespCommand<'_>,
    now_ms: u64,
    out: &mut Vec<u8>,
) {
    if command.argc() == 0 {
        wrong_arity(out, b"del");
        return;
    }
    if !same_slot(command.args()) {
        write_error(out, ERR_CROSS_SLOT);
        return;
    }

    let mut deleted = 0i64;
    for key in command.args() {
        if store.delete_at(key, now_ms) {
            deleted += 1;
        }
    }
    write_integer(out, deleted);
}

fn execute_exists(
    store: &mut CacheStore,
    command: RespCommand<'_>,
    now_ms: u64,
    out: &mut Vec<u8>,
) {
    if command.argc() == 0 {
        wrong_arity(out, b"exists");
        return;
    }
    if !same_slot(command.args()) {
        write_error(out, ERR_CROSS_SLOT);
        return;
    }

    let mut count = 0i64;
    for key in command.args() {
        if store.exists(key, now_ms) {
            count += 1;
        }
    }
    write_integer(out, count);
}

fn execute_incr(
    store: &mut CacheStore,
    command: RespCommand<'_>,
    now_ms: u64,
    out: &mut Vec<u8>,
) {
    if command.argc() != 1 {
        wrong_arity(out, b"incr");
        return;
    }

    let key = command.args().next().expect("validated argument");
    match store.increment(key, 1, now_ms) {
        Ok(value) => write_integer(out, value),
        Err(CacheIncrementError::NotInteger | CacheIncrementError::Overflow) => {
            write_error(out, ERR_INTEGER)
        }
    }
}

fn execute_expire(
    store: &mut CacheStore,
    command: RespCommand<'_>,
    now_ms: u64,
    out: &mut Vec<u8>,
) {
    if command.argc() != 2 {
        wrong_arity(out, b"expire");
        return;
    }

    let mut args = command.args();
    let key = args.next().expect("validated key");
    let Some(seconds) = parse_i64(args.next().expect("validated ttl")) else {
        write_error(out, ERR_INTEGER);
        return;
    };

    let changed = if seconds <= 0 {
        store.delete_at(key, now_ms)
    } else {
        let Some(ttl_ms) = (seconds as u64).checked_mul(1_000) else {
            write_error(out, ERR_INTEGER);
            return;
        };
        store.expire_ms(key, ttl_ms, now_ms)
    };
    write_integer(out, i64::from(changed));
}

fn execute_ttl(
    store: &mut CacheStore,
    command: RespCommand<'_>,
    now_ms: u64,
    out: &mut Vec<u8>,
) {
    if command.argc() != 1 {
        wrong_arity(out, b"ttl");
        return;
    }

    let key = command.args().next().expect("validated argument");
    let ttl = match store.ttl(key, now_ms) {
        CacheTtl::Missing => -2,
        CacheTtl::Persistent => -1,
        CacheTtl::RemainingMs(ms) => (ms / 1_000) as i64,
    };
    write_integer(out, ttl);
}

fn execute_mget(
    store: &mut CacheStore,
    command: RespCommand<'_>,
    now_ms: u64,
    out: &mut Vec<u8>,
) {
    if command.argc() == 0 {
        wrong_arity(out, b"mget");
        return;
    }
    if !same_slot(command.args()) {
        write_error(out, ERR_CROSS_SLOT);
        return;
    }

    write_array_len(out, command.argc());
    for key in command.args() {
        write_value(store.get(key, now_ms), out);
    }
}

fn execute_mset(
    store: &mut CacheStore,
    command: RespCommand<'_>,
    now_ms: u64,
    out: &mut Vec<u8>,
) {
    if command.argc() == 0 || command.argc() % 2 != 0 {
        wrong_arity(out, b"mset");
        return;
    }
    if !same_slot_pairs(command.args()) {
        write_error(out, ERR_CROSS_SLOT);
        return;
    }

    // The shard owner executes a command to completion without yielding. Once
    // all keys have passed the slot check, these writes are atomic with respect
    // to other commands on this shard.
    let mut args = command.args();
    while let Some(key) = args.next() {
        let value = args.next().expect("validated value pair");
        store.set_bytes(key, value, None, now_ms);
    }
    write_simple(out, b"OK");
}

fn write_value(value: Option<CacheValueView<'_>>, out: &mut Vec<u8>) {
    match value {
        Some(CacheValueView::Bytes(value)) => write_bulk(out, value),
        Some(CacheValueView::Integer(value)) => write_bulk_integer(out, value),
        None => write_null_bulk(out),
    }
}

fn parse_i64(value: &[u8]) -> Option<i64> {
    std::str::from_utf8(value).ok()?.parse().ok()
}

fn same_slot(mut keys: RespArgs<'_>) -> bool {
    let Some(first) = keys.next() else {
        return true;
    };
    let slot = redis_slot(first);
    keys.all(|key| redis_slot(key) == slot)
}

fn same_slot_pairs(mut args: RespArgs<'_>) -> bool {
    let Some(first_key) = args.next() else {
        return true;
    };
    let _first_value = args.next();
    let slot = redis_slot(first_key);

    while let Some(key) = args.next() {
        let _value = args.next();
        if redis_slot(key) != slot {
            return false;
        }
    }
    true
}

fn wrong_arity(out: &mut Vec<u8>, command: &[u8]) {
    out.extend_from_slice(b"-ERR wrong number of arguments for '");
    out.extend_from_slice(command);
    out.extend_from_slice(b"' command\r\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(store: &mut CacheStore, frame: &[u8], now_ms: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let consumed = execute_frame(store, frame, now_ms, &mut out)
            .unwrap()
            .expect("complete command");
        assert_eq!(consumed, frame.len());
        out
    }

    #[test]
    fn ping_get_set_and_incr_round_trip() {
        let mut store = CacheStore::new();
        assert_eq!(run(&mut store, b"*1\r\n$4\r\nPING\r\n", 0), b"+PONG\r\n");
        assert_eq!(
            run(
                &mut store,
                b"*3\r\n$3\r\nSET\r\n$1\r\nn\r\n$2\r\n41\r\n",
                0
            ),
            b"+OK\r\n"
        );
        assert_eq!(
            run(&mut store, b"*2\r\n$4\r\nINCR\r\n$1\r\nn\r\n", 0),
            b":42\r\n"
        );
        assert_eq!(
            run(&mut store, b"*2\r\n$3\r\nGET\r\n$1\r\nn\r\n", 0),
            b"$2\r\n42\r\n"
        );
    }

    #[test]
    fn set_px_expire_and_ttl_follow_live_key_time() {
        let mut store = CacheStore::new();
        assert_eq!(
            run(
                &mut store,
                b"*5\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n$2\r\nPX\r\n$4\r\n2500\r\n",
                100
            ),
            b"+OK\r\n"
        );
        assert_eq!(
            run(&mut store, b"*2\r\n$3\r\nTTL\r\n$1\r\nk\r\n", 600),
            b":2\r\n"
        );
        assert_eq!(
            run(&mut store, b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n", 2_600),
            b"$-1\r\n"
        );
        assert_eq!(
            run(&mut store, b"*2\r\n$3\r\nTTL\r\n$1\r\nk\r\n", 2_600),
            b":-2\r\n"
        );
    }

    #[test]
    fn mset_and_mget_are_same_slot_atomic_commands() {
        let mut store = CacheStore::new();
        assert_eq!(
            run(
                &mut store,
                b"*5\r\n$4\r\nMSET\r\n$5\r\na{42}\r\n$1\r\n1\r\n$5\r\nb{42}\r\n$1\r\n2\r\n",
                0
            ),
            b"+OK\r\n"
        );
        assert_eq!(
            run(
                &mut store,
                b"*3\r\n$4\r\nMGET\r\n$5\r\na{42}\r\n$5\r\nb{42}\r\n",
                0
            ),
            b"*2\r\n$1\r\n1\r\n$1\r\n2\r\n"
        );
    }

    #[test]
    fn cross_slot_mset_fails_before_mutating() {
        let mut store = CacheStore::new();
        assert_eq!(
            run(
                &mut store,
                b"*5\r\n$4\r\nMSET\r\n$1\r\na\r\n$1\r\n1\r\n$1\r\nb\r\n$1\r\n2\r\n",
                0
            ),
            b"-CROSSSLOT Keys in request don't hash to the same slot\r\n"
        );
        assert!(store.is_empty());
    }

    #[test]
    fn expire_and_exists_observe_logical_expiry() {
        let mut store = CacheStore::new();
        run(
            &mut store,
            b"*3\r\n$3\r\nSET\r\n$4\r\nkey1\r\n$1\r\nv\r\n",
            0,
        );
        assert_eq!(
            run(
                &mut store,
                b"*3\r\n$6\r\nEXPIRE\r\n$4\r\nkey1\r\n$1\r\n1\r\n",
                0
            ),
            b":1\r\n"
        );
        assert_eq!(
            run(
                &mut store,
                b"*2\r\n$6\r\nEXISTS\r\n$4\r\nkey1\r\n",
                1_000
            ),
            b":0\r\n"
        );
    }

    #[test]
    fn execute_frame_leaves_pipeline_tail_to_caller() {
        let mut store = CacheStore::new();
        let first = b"*1\r\n$4\r\nPING\r\n";
        let second = b"*2\r\n$3\r\nGET\r\n$1\r\nx\r\n";
        let mut input = first.to_vec();
        input.extend_from_slice(second);

        let mut out = Vec::new();
        let consumed = execute_frame(&mut store, &input, 0, &mut out)
            .unwrap()
            .unwrap();
        assert_eq!(consumed, first.len());
        assert_eq!(out, b"+PONG\r\n");
    }
}
