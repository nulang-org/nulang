# collections-ext

Deque, ordered-map, and priority-queue collections built on plain arrays.
Official seed package (Experimental tier).

## Install

```bash
nula add collections-ext
```

## Usage

```nulang
import lib
```

All collections are immutable-style values: every update returns a new
collection, so old snapshots stay valid.

### Deque (double-ended queue)

A deque is an array used through the `dq_*` helpers.

```nulang
let d = dq_new();
let d2 = dq_push_back(d, 1);
let d3 = dq_push_front(d2, 0);
let p = dq_pop_front(d3);  // (0, rest)
```

| Function | Description |
| --- | --- |
| `dq_new()` | empty deque |
| `dq_push_front(d, x)` / `dq_push_back(d, x)` | add at either end |
| `dq_pop_front(d)` / `dq_pop_back(d)` | `(value, new_deque)`; value is `Option` (`None` when empty) |
| `dq_peek_front(d)` / `dq_peek_back(d)` | `Option` of the end element |
| `dq_size(d)` / `dq_is_empty(d)` | size helpers |
| `dq_to_array(d)` | underlying array, front first |

### Ordered map (string keys, kept sorted)

```nulang
let m = om_insert("b", 2, om_new());
let m2 = om_insert("a", 1, m);
om_get("a", m2)      // Some(1)
om_keys(m2)          // ["a", "b"]
```

| Function | Description |
| --- | --- |
| `om_new()` | empty ordered map |
| `om_insert(k, v, m)` | insert or overwrite, stays sorted |
| `om_get(k, m)` | `Option` of the value |
| `om_contains(k, m)` / `om_remove(k, m)` | membership / removal |
| `om_keys(m)` / `om_values(m)` | sorted keys / values in key order |
| `om_size(m)` / `om_is_empty(m)` | size helpers |

### Priority queue (min-heap)

Elements are dequeued in ascending priority order.

```nulang
let q = pq_push(5, "low", pq_new());
let q2 = pq_push(1, "high", q);
pq_pop(q2)  // (Some("high"), rest)
```

| Function | Description |
| --- | --- |
| `pq_new()` | empty queue |
| `pq_push(pri, v, q)` | enqueue with integer priority |
| `pq_pop(q)` | `(Option value, new_queue)` with the smallest priority |
| `pq_peek(q)` | `Option` of the smallest-priority value |
| `pq_peek_priority(q)` | `Option` of the smallest priority |
| `pq_size(q)` / `pq_is_empty(q)` | size helpers |

## Tests

```bash
nula test
```
