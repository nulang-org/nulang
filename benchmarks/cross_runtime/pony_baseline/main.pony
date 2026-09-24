use "time"

actor Counter
  let _main: Main tag
  let _target: U64
  var _count: U64 = 0

  new create(main: Main tag, target: U64) =>
    _main = main
    _target = target

  be inc() =>
    _count = _count + 1
    if _count == _target then
      _main.count_done(_count)
    end

actor Pong
  var _count: U64 = 0

  be recv(ping: Ping tag) =>
    _count = _count + 1
    ping.ack(_count, this)

actor Ping
  let _main: Main tag
  var _remaining: U64 = 0

  new create(main: Main tag) =>
    _main = main

  be kick(n: U64, pong: Pong tag) =>
    _remaining = n
    pong.recv(this)

  be ack(count: U64, pong: Pong tag) =>
    _remaining = _remaining - 1
    if _remaining == 0 then
      _main.ping_done(count)
    else
      pong.recv(this)
    end

actor RingNode
  let _main: Main tag
  var _next: (RingNode tag | None) = None

  new create(main: Main tag) =>
    _main = main

  be setup(next: RingNode tag) =>
    _next = next
    _main.ring_ready()

  be token(hops: U64, count: U64) =>
    if hops > 0 then
      match _next
      | let next: RingNode tag => next.token(hops - 1, count + 1)
      end
    else
      _main.ring_done(count)
    end

  be stop(remaining: U64) =>
    let next = _next
    _next = None
    if remaining > 1 then
      match next
      | let n: RingNode tag => n.stop(remaining - 1)
      end
    end

actor ForkSink
  let _main: Main tag
  let _target: U64
  var _count: U64 = 0

  new create(main: Main tag, target: U64) =>
    _main = main
    _target = target

  be ack() =>
    _count = _count + 1
    if _count == _target then
      _main.fork_done(_count)
    end

actor ForkWorker
  let _sink: ForkSink tag

  new create(sink: ForkSink tag) =>
    _sink = sink

  be task() =>
    _sink.ack()

actor Main
  let _env: Env
  var _start: U64 = 0
  var _ring_ready_count: U64 = 0
  var _ring_first: (RingNode tag | None) = None

  let _count_n: U64 = 200000
  let _ping_n: U64 = 20000
  let _ring_n: USize = 10
  let _hops: U64 = 20000
  let _workers: USize = 8
  let _tasks: U64 = 50000

  new create(env: Env) =>
    _env = env
    _run_counting()

  fun ref _fail(message: String val) =>
    _env.err.print(message)
    _env.exitcode(1)

  fun ref _report(name: String val, messages: U64, elapsed: U64) =>
    _env.out.print(
      "[cross-bench] runtime=pony benchmark=" + name +
      " messages=" + messages.string() +
      " elapsed_ns=" + elapsed.string())

  fun ref _run_counting() =>
    let counter = Counter(this, _count_n)
    _start = Time.nanos()
    var i: U64 = 0
    while i < _count_n do
      counter.inc()
      i = i + 1
    end

  be count_done(count: U64) =>
    let elapsed = Time.nanos() - _start
    if count != _count_n then
      _fail("pony counting lost messages")
    else
      _report("counting", _count_n, elapsed)
      _run_ping_pong()
    end

  fun ref _run_ping_pong() =>
    let ping = Ping(this)
    let pong = Pong
    _start = Time.nanos()
    ping.kick(_ping_n, pong)

  be ping_done(count: U64) =>
    let elapsed = Time.nanos() - _start
    if count != _ping_n then
      _fail("pony ping-pong lost messages")
    else
      _report("ping_pong", (2 * _ping_n) + 1, elapsed)
      _run_ring()
    end

  fun ref _run_ring() =>
    let nodes = Array[RingNode tag](_ring_n)
    var i: USize = 0
    while i < _ring_n do
      nodes.push(RingNode(this))
      i = i + 1
    end

    try
      _ring_first = nodes(0)?
      i = 0
      while i < _ring_n do
        let next_idx = (i + 1) % _ring_n
        nodes(i)?.setup(nodes(next_idx)?)
        i = i + 1
      end
    else
      _fail("pony ring construction failed")
    end

  be ring_ready() =>
    _ring_ready_count = _ring_ready_count + 1
    if _ring_ready_count.usize() == _ring_n then
      match _ring_first
      | let first: RingNode tag =>
        _start = Time.nanos()
        first.token(_hops, 0)
      else
        _fail("pony ring first actor missing")
      end
    end

  be ring_done(total: U64) =>
    let elapsed = Time.nanos() - _start
    if total != _hops then
      _fail("pony thread ring returned wrong hop count")
    else
      _report("thread_ring", _hops, elapsed)
      match _ring_first
      | let first: RingNode tag => first.stop(_ring_n.u64())
      end
      _ring_first = None
      _run_fork_join()
    end

  fun ref _run_fork_join() =>
    let sink = ForkSink(this, _tasks)
    let workers = Array[ForkWorker tag](_workers)
    var i: USize = 0
    while i < _workers do
      workers.push(ForkWorker(sink))
      i = i + 1
    end

    _start = Time.nanos()
    var task: U64 = 0
    while task < _tasks do
      try
        workers((task % _workers.u64()).usize())?.task()
      else
        _fail("pony fork-join worker lookup failed")
        return
      end
      task = task + 1
    end

  be fork_done(count: U64) =>
    let elapsed = Time.nanos() - _start
    if count != _tasks then
      _fail("pony fork-join lost tasks")
    else
      _report("fork_join", 2 * _tasks, elapsed)
    end
