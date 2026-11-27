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
const ERR_OOM: &[u8] = b"OOM cache admission limit exceeded";
const ERR_UNKNOWN: &[u8] = b"ERR unknown command";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RespCommandSlot {
    Unkeyed,
    Slot(u16),
    CrossSlot,
}

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

pub fn command_slot(command: RespCommand<'_>) -> RespCommandSlot {
    let name = command.name();

    if name.eq_ignore_ascii_case(b"GET")
        || name.eq_ignore_ascii_case(b"INCR")
        || name.eq_ignore_ascii_case(b"TTL")
    {
        return if command.argc() == 1 {
            RespCommandSlot::Slot(redis_slot(
                command.args().next().expect("validated routing key"),
            ))
        } else {
            RespCommandSlot::Unkeyed
        };
    }

    if name.eq_ignore_ascii_case(b"SET") {
        return if command.argc() == 2 || command.argc() == 4 {
            RespCommandSlot::Slot(redis_slot(
                command.args().next().expect("validated routing key"),
            ))
        } else {
            RespCommandSlot::Unkeyed
        };
    }

    if name.eq_ignore_ascii_case(b"EXPIRE") {
        return if command.argc() == 2 {
            RespCommandSlot::Slot(redis_slot(
                command.args().next().expect("validated routing key"),
            ))
        } else {
            RespCommandSlot::Unkeyed
        };
    }

    if name.eq_ignore_ascii_case(b"DEL")
        || name.eq_ignore_ascii_case(b"EXISTS")
        || name.eq_ignore_ascii_case(b"MGET")
    {
        return if command.argc() == 0 {
            RespCommandSlot::Unkeyed
        } else {
            route_key_args(command.args())
        };
    }

    if name.eq_ignore_ascii_case(b"MSET") {
        return if command.argc() == 0 || !command.argc().is_multiple_of(2) {
            RespCommandSlot::Unkeyed
        } else {
            route_key_value_args(command.args())
        };
    }

    RespCommandSlot::Unkeyed
}

fn route_key_args(mut keys: RespArgs<'_>) -> RespCommandSlot {
    let Some(first) = keys.next() else {
        return RespCommandSlot::Unkeyed;
    };
    let slot = redis_slot(first);
    if keys.all(|key| redis_slot(key) == slot) {
        RespCommandSlot::Slot(slot)
    } else {
        RespCommandSlot::CrossSlot
    }
}

fn route_key_value_args(mut args: RespArgs<'_>) -> RespCommandSlot {
    let Some(first_key) = args.next() else {
        return RespCommandSlot::Unkeyed;
    };
    let _first_value = args.next();
    let slot = redis_slot(first_key);

    while let Some(key) = args.next() {
        let _value = args.next();
        if redis_slot(key) != slot {
            return RespCommandSlot::CrossSlot;
        }
    }

    RespCommandSlot::Slot(slot)
}

// Static dispatch keeps the memory-only path free of journal work while sharing
// all protocol validation and response encoding with the durable backend.
trait RespBackend {
    fn get(&mut self, key: &[u8], now: u64) -> Result<Option<CacheValueView<'_>>, &'static [u8]>;
    fn exists(&mut self, key: &[u8], now: u64) -> Result<bool, &'static [u8]>;
    fn ttl(&mut self, key: &[u8], now: u64) -> Result<CacheTtl, &'static [u8]>;
    fn set_bytes(
        &mut self,
        key: &[u8],
        value: &[u8],
        ttl: Option<u64>,
        now: u64,
    ) -> Result<(), &'static [u8]>;
    fn set_batch(&mut self, pairs: &[(&[u8], &[u8])], now: u64) -> Result<(), &'static [u8]>;
    fn delete_many(&mut self, keys: RespArgs<'_>, now: u64) -> Result<usize, &'static [u8]>;
    fn increment(&mut self, key: &[u8], now: u64) -> Result<i64, &'static [u8]>;
    fn expire(&mut self, key: &[u8], ttl: u64, now: u64) -> Result<bool, &'static [u8]>;
}

fn increment_error(error: CacheIncrementError) -> &'static [u8] {
    match error {
        CacheIncrementError::Admission(_) => ERR_OOM,
        CacheIncrementError::NotInteger => ERR_INTEGER,
        CacheIncrementError::Overflow => b"ERR increment or decrement would overflow",
    }
}

impl RespBackend for CacheStore {
    fn get(&mut self, key: &[u8], now: u64) -> Result<Option<CacheValueView<'_>>, &'static [u8]> {
        Ok(CacheStore::get(self, key, now))
    }
    fn exists(&mut self, key: &[u8], now: u64) -> Result<bool, &'static [u8]> {
        Ok(CacheStore::exists(self, key, now))
    }
    fn ttl(&mut self, key: &[u8], now: u64) -> Result<CacheTtl, &'static [u8]> {
        Ok(CacheStore::ttl(self, key, now))
    }
    fn set_bytes(
        &mut self,
        key: &[u8],
        value: &[u8],
        ttl: Option<u64>,
        now: u64,
    ) -> Result<(), &'static [u8]> {
        CacheStore::set_bytes(self, key, value, ttl, now).map_err(|_| ERR_OOM)
    }
    fn set_batch(&mut self, pairs: &[(&[u8], &[u8])], now: u64) -> Result<(), &'static [u8]> {
        CacheStore::set_batch(self, pairs, now).map_err(|_| ERR_OOM)
    }
    fn delete_many(&mut self, keys: RespArgs<'_>, now: u64) -> Result<usize, &'static [u8]> {
        Ok(keys.filter(|key| self.delete_at(key, now)).count())
    }
    fn increment(&mut self, key: &[u8], now: u64) -> Result<i64, &'static [u8]> {
        CacheStore::increment(self, key, 1, now).map_err(increment_error)
    }
    fn expire(&mut self, key: &[u8], ttl: u64, now: u64) -> Result<bool, &'static [u8]> {
        Ok(if ttl == 0 {
            self.delete_at(key, now)
        } else {
            self.expire_ms(key, ttl, now)
        })
    }
}

#[cfg(all(unix, feature = "cache-server"))]
use super::cache_durable::{CacheTime, DurableCache, DurableError};

#[cfg(all(unix, feature = "cache-server"))]
fn durable_error(error: DurableError) -> &'static [u8] {
    match error {
        DurableError::Admission(_) => ERR_OOM,
        DurableError::Increment(error) => increment_error(error),
        DurableError::InvalidTime => ERR_INTEGER,
        DurableError::Journal(_) | DurableError::Poisoned => b"ERR cache persistence unavailable",
    }
}

#[cfg(all(unix, feature = "cache-server"))]
struct DurableResp<'a> {
    store: &'a mut DurableCache,
    time: CacheTime,
}
#[cfg(all(unix, feature = "cache-server"))]
impl RespBackend for DurableResp<'_> {
    fn get(&mut self, key: &[u8], now: u64) -> Result<Option<CacheValueView<'_>>, &'static [u8]> {
        self.store.get(key, now).map_err(durable_error)
    }
    fn exists(&mut self, key: &[u8], now: u64) -> Result<bool, &'static [u8]> {
        self.store.exists(key, now).map_err(durable_error)
    }
    fn ttl(&mut self, key: &[u8], now: u64) -> Result<CacheTtl, &'static [u8]> {
        self.store.ttl(key, now).map_err(durable_error)
    }
    fn set_bytes(
        &mut self,
        key: &[u8],
        value: &[u8],
        ttl: Option<u64>,
        _: u64,
    ) -> Result<(), &'static [u8]> {
        self.store
            .set_bytes(key, value, ttl, self.time)
            .map_err(durable_error)
    }
    fn set_batch(&mut self, pairs: &[(&[u8], &[u8])], _: u64) -> Result<(), &'static [u8]> {
        self.store
            .set_batch(pairs, self.time)
            .map_err(durable_error)
    }
    fn delete_many(&mut self, keys: RespArgs<'_>, _: u64) -> Result<usize, &'static [u8]> {
        self.store
            .delete_many(&keys.collect::<Vec<_>>(), self.time)
            .map_err(durable_error)
    }
    fn increment(&mut self, key: &[u8], _: u64) -> Result<i64, &'static [u8]> {
        self.store
            .increment(key, 1, self.time)
            .map_err(durable_error)
    }
    fn expire(&mut self, key: &[u8], ttl: u64, _: u64) -> Result<bool, &'static [u8]> {
        self.store
            .expire(key, ttl, self.time)
            .map_err(durable_error)
    }
}

#[cfg(all(unix, feature = "cache-server"))]
pub(super) fn execute_durable_command(
    store: &mut DurableCache,
    command: RespCommand<'_>,
    time: CacheTime,
    out: &mut Vec<u8>,
) -> Result<(), DurableError> {
    if store.is_poisoned() {
        return Err(DurableError::Poisoned);
    }
    let start = out.len();
    execute_command_inner(
        &mut DurableResp { store, time },
        command,
        time.monotonic_ms,
        out,
    );
    if store.is_poisoned() {
        out.truncate(start);
        return Err(DurableError::Poisoned);
    }
    Ok(())
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
    execute_command_inner(store, command, now_ms, out);
}

fn execute_command_inner(
    store: &mut impl RespBackend,
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
    store: &mut impl RespBackend,
    command: RespCommand<'_>,
    now_ms: u64,
    out: &mut Vec<u8>,
) {
    if command.argc() != 1 {
        wrong_arity(out, b"get");
        return;
    }

    let key = command.args().next().expect("validated argument");
    match store.get(key, now_ms) {
        Ok(value) => write_value(value, out),
        Err(error) => write_error(out, error),
    }
}

fn execute_set(
    store: &mut impl RespBackend,
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

    match store.set_bytes(key, value, ttl_ms, now_ms) {
        Ok(()) => write_simple(out, b"OK"),
        Err(error) => write_error(out, error),
    }
}

fn execute_del(
    store: &mut impl RespBackend,
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

    match store.delete_many(command.args(), now_ms) {
        Ok(deleted) => write_integer(out, deleted as i64),
        Err(error) => write_error(out, error),
    }
}

fn execute_exists(
    store: &mut impl RespBackend,
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
        match store.exists(key, now_ms) {
            Ok(exists) => count += i64::from(exists),
            Err(error) => {
                write_error(out, error);
                return;
            }
        }
    }
    write_integer(out, count);
}

fn execute_incr(
    store: &mut impl RespBackend,
    command: RespCommand<'_>,
    now_ms: u64,
    out: &mut Vec<u8>,
) {
    if command.argc() != 1 {
        wrong_arity(out, b"incr");
        return;
    }

    let key = command.args().next().expect("validated argument");
    match store.increment(key, now_ms) {
        Ok(value) => write_integer(out, value),
        Err(error) => write_error(out, error),
    }
}

fn execute_expire(
    store: &mut impl RespBackend,
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

    let ttl = if seconds <= 0 {
        0
    } else {
        let Some(ttl_ms) = (seconds as u64).checked_mul(1_000) else {
            write_error(out, ERR_INTEGER);
            return;
        };
        ttl_ms
    };
    match store.expire(key, ttl, now_ms) {
        Ok(changed) => write_integer(out, i64::from(changed)),
        Err(error) => write_error(out, error),
    }
}

fn execute_ttl(
    store: &mut impl RespBackend,
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
        Ok(CacheTtl::Missing) => -2,
        Ok(CacheTtl::Persistent) => -1,
        // Redis rounds to the nearest second. Divide first to avoid overflow.
        Ok(CacheTtl::RemainingMs(ms)) => (ms / 1_000 + u64::from(ms % 1_000 >= 500)) as i64,
        Err(error) => {
            write_error(out, error);
            return;
        }
    };
    write_integer(out, ttl);
}

fn execute_mget(
    store: &mut impl RespBackend,
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

    let start = out.len();
    write_array_len(out, command.argc());
    for key in command.args() {
        match store.get(key, now_ms) {
            Ok(value) => write_value(value, out),
            Err(error) => {
                out.truncate(start);
                write_error(out, error);
                return;
            }
        }
    }
}

fn execute_mset(
    store: &mut impl RespBackend,
    command: RespCommand<'_>,
    now_ms: u64,
    out: &mut Vec<u8>,
) {
    if command.argc() == 0 || !command.argc().is_multiple_of(2) {
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
    let mut pairs = Vec::new();
    while let Some(key) = args.next() {
        pairs.push((key, args.next().expect("validated value pair")));
    }
    match store.set_batch(&pairs, now_ms) {
        Ok(()) => write_simple(out, b"OK"),
        Err(error) => write_error(out, error),
    }
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
            run(&mut store, b"*3\r\n$3\r\nSET\r\n$1\r\nn\r\n$2\r\n41\r\n", 0),
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
            run(&mut store, b"*2\r\n$6\r\nEXISTS\r\n$4\r\nkey1\r\n", 1_000),
            b":0\r\n"
        );
    }

    #[test]
    fn command_slot_distinguishes_keyed_unkeyed_and_cross_slot_commands() {
        let (ping, _) = parse_command(b"*1\r\n$4\r\nPING\r\n").unwrap().unwrap();
        assert_eq!(command_slot(ping), RespCommandSlot::Unkeyed);

        let (get, _) = parse_command(b"*2\r\n$3\r\nGET\r\n$5\r\na{42}\r\n")
            .unwrap()
            .unwrap();
        assert_eq!(
            command_slot(get),
            RespCommandSlot::Slot(redis_slot(b"a{42}"))
        );

        let (mget, _) = parse_command(b"*3\r\n$4\r\nMGET\r\n$5\r\na{42}\r\n$5\r\nb{42}\r\n")
            .unwrap()
            .unwrap();
        assert_eq!(
            command_slot(mget),
            RespCommandSlot::Slot(redis_slot(b"a{42}"))
        );

        let (cross, _) = parse_command(b"*3\r\n$4\r\nMGET\r\n$1\r\na\r\n$1\r\nb\r\n")
            .unwrap()
            .unwrap();
        assert_eq!(command_slot(cross), RespCommandSlot::CrossSlot);
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

    #[test]
    fn admission_rejects_set_and_incr_without_mutating_existing_keys() {
        let mut store = CacheStore::with_limits(super::super::cache::CacheLimits {
            max_entries: 1,
            max_arena_bytes: 32,
        });
        let large = vec![b'x'; 32];
        let cmd = |args: &[&[u8]]| {
            let mut wire = format!("*{}\r\n", args.len()).into_bytes();
            for arg in args {
                wire.extend_from_slice(format!("{}{}\r\n", '$', arg.len()).as_bytes());
                wire.extend_from_slice(arg);
                wire.extend_from_slice(b"\r\n");
            }
            wire
        };
        assert_eq!(
            run(&mut store, &cmd(&[b"SET", b"a", b"old"]), 0),
            b"+OK\r\n"
        );
        assert!(run(&mut store, &cmd(&[b"SET", b"b", b"v"]), 0).starts_with(b"-OOM "));
        assert!(run(&mut store, &cmd(&[b"INCR", b"b"]), 0).starts_with(b"-OOM "));
        assert_eq!(
            run(&mut store, &cmd(&[b"SET", b"a", &large]), 0),
            b"+OK\r\n"
        );
        assert!(run(&mut store, &cmd(&[b"SET", b"a", &[b'y'; 33]]), 0).starts_with(b"-OOM "));
        assert_eq!(store.get(b"a", 0), Some(CacheValueView::Bytes(&large)));
        assert_eq!(store.len(), 1);
        assert_eq!(store.memory_stats().arena_reserved_bytes, 32);
        assert!(store.delete(b"a"));
        assert_eq!(
            run(&mut store, &cmd(&[b"SET", b"b", &large]), 0),
            b"+OK\r\n"
        );
    }

    #[test]
    fn admission_rejects_whole_mset_before_mutation() {
        let mut store = CacheStore::with_limits(super::super::cache::CacheLimits {
            max_entries: 1,
            max_arena_bytes: 32,
        });
        run(
            &mut store,
            b"*3\r\n$3\r\nSET\r\n$4\r\na{t}\r\n$3\r\nold\r\n",
            0,
        );
        assert!(run(
            &mut store,
            b"*5\r\n$4\r\nMSET\r\n$4\r\na{t}\r\n$1\r\nx\r\n$4\r\nb{t}\r\n$1\r\ny\r\n",
            0
        )
        .starts_with(b"-OOM "));
        assert_eq!(store.get(b"a{t}", 0), Some(CacheValueView::Bytes(b"old")));
        assert_eq!(store.len(), 1);
        assert_eq!(
            run(
                &mut store,
                b"*5\r\n$4\r\nMSET\r\n$4\r\na{t}\r\n$1\r\nx\r\n$4\r\na{t}\r\n$1\r\ny\r\n",
                0
            ),
            b"+OK\r\n"
        );
        assert_eq!(store.get(b"a{t}", 0), Some(CacheValueView::Bytes(b"y")));
    }

    #[cfg(all(unix, feature = "cache-server"))]
    #[test]
    fn test_durable_and_memory_resp_parity_across_checkpoints_and_restarts() {
        use super::super::cache::{CacheLimits, CacheStore};
        use super::super::cache_wal::{WalLimits, WalSync};
        use std::path::PathBuf;
        struct Temp(PathBuf);
        impl Drop for Temp {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let tmp = Temp(std::env::temp_dir().join(format!(
                "nulang-parity-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )));
        std::fs::create_dir(&tmp.0).unwrap();
        let path = tmp.0.join("journal");
        let keys = [b"a{t}".as_slice(), b"b{t}", b"c{t}", b"d{t}"];
        let values = [
            b"0".as_slice(),
            b"41",
            b"-1",
            b"+1",
            b"01",
            b"9223372036854775807",
            b"\0\xff",
            b"",
        ];
        for (i, seed) in [1u64, 42, 0xdeadbeef].into_iter().enumerate() {
            let path = path.with_extension(format!("{i}.wal"));
            let mut memory = CacheStore::new();
            let mut state = seed;
            let mut anchor = 0u64;
            let open = |absolute: u64| {
                DurableCache::open(
                    &path,
                    CacheLimits::default(),
                    WalLimits::default(),
                    WalSync::Buffered,
                    CacheTime {
                        monotonic_ms: 0,
                        unix_ms: 1_000_000 + absolute,
                    },
                )
                .unwrap()
            };
            let mut durable = open(0);
            for step in 0..256u64 {
                let now = step * 53;
                if step != 0 && step % 32 == 0 {
                    durable
                        .checkpoint(CacheTime {
                            monotonic_ms: now - anchor,
                            unix_ms: 1_000_000 + now,
                        })
                        .unwrap();
                    drop(durable);
                    durable = open(now);
                    anchor = now;
                }
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let key = keys[(state >> 8) as usize % keys.len()];
                let other = keys[(state >> 16) as usize % keys.len()];
                let value = values[(state >> 24) as usize % values.len()];
                let parts: &[&[u8]] = match state % 14 {
                    0 => &[b"SET", key, value],
                    1 => &[b"SET", key, value, b"PX", b"200"],
                    2 => &[b"GET", key],
                    3 => &[b"INCR", key],
                    4 => &[b"DEL", key, other],
                    5 => &[b"EXPIRE", key, b"1"],
                    6 => &[b"EXPIRE", key, b"0"],
                    7 => &[b"TTL", key],
                    8 => &[b"MSET", key, value, other, b"42"],
                    9 => &[b"MGET", key, other],
                    10 => &[b"EXISTS", key, other],
                    11 => &[b"SET", key, value, b"PX", b"0"],
                    12 => &[b"MSET", key, value, b"unrelated", b"no"],
                    _ => &[b"PING", value],
                };
                let mut wire = format!("*{}\r\n", parts.len()).into_bytes();
                for part in parts {
                    wire.extend_from_slice(format!("${}\r\n", part.len()).as_bytes());
                    wire.extend_from_slice(part);
                    wire.extend_from_slice(b"\r\n");
                }
                let expected = run(&mut memory, &wire, now);
                let (cmd, consumed) = parse_command(&wire).unwrap().unwrap();
                assert_eq!(consumed, wire.len());
                let mut actual = Vec::new();
                execute_durable_command(
                    &mut durable,
                    cmd,
                    CacheTime {
                        monotonic_ms: now - anchor,
                        unix_ms: 1_000_000 + now,
                    },
                    &mut actual,
                )
                .unwrap();
                assert_eq!(actual, expected, "seed {seed}, step {step}, args {parts:?}");
            }
        }
    }
}
