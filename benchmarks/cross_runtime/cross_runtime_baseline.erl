-module(cross_runtime_baseline).
-export([main/0]).

-define(COUNT_N, 200000).
-define(PING_N, 20000).
-define(RING_N, 10).
-define(HOPS, 20000).
-define(WORKERS, 8).
-define(TASKS, 50000).

report(Name, Messages, ElapsedNs) ->
    io:format("[cross-bench] runtime=erlang benchmark=~s messages=~B elapsed_ns=~B~n",
              [Name, Messages, ElapsedNs]).

send_n(_Pid, 0, _Msg) -> ok;
send_n(Pid, N, Msg) ->
    Pid ! Msg,
    send_n(Pid, N - 1, Msg).

counting_loop(Parent, Count) ->
    receive
        inc ->
            Next = Count + 1,
            case Next of
                ?COUNT_N -> Parent ! {count_done, Next};
                _ -> counting_loop(Parent, Next)
            end;
        stop -> ok
    end.

counting() ->
    Actor = spawn(fun() -> counting_loop(self(), 0) end),
    %% Spawn closure's self() is the actor, so pass the parent explicitly.
    exit(Actor, kill),
    Parent = self(),
    Counter = spawn(fun() -> counting_loop(Parent, 0) end),
    Start = erlang:monotonic_time(nanosecond),
    send_n(Counter, ?COUNT_N, inc),
    Count = receive {count_done, C} -> C end,
    Elapsed = erlang:monotonic_time(nanosecond) - Start,
    true = (Count =:= ?COUNT_N),
    Counter ! stop,
    report("counting", ?COUNT_N, Elapsed).

ping_loop(Pong, Parent, Remaining) ->
    receive
        {kick, N} ->
            Pong ! recv,
            ping_loop(Pong, Parent, N);
        {ack, PongCount} ->
            Next = Remaining - 1,
            case Next of
                0 ->
                    Parent ! {ping_done, PongCount},
                    ping_loop(Pong, Parent, 0);
                _ ->
                    Pong ! recv,
                    ping_loop(Pong, Parent, Next)
            end;
        stop -> ok
    end.

pong_loop(Ping, Count) ->
    receive
        recv ->
            Next = Count + 1,
            Ping ! {ack, Next},
            pong_loop(Ping, Next);
        stop -> ok
    end.

ping_pong() ->
    Parent = self(),
    %% Use a relay so both actor pids can be constructed without global names.
    Relay = spawn(fun relay_loop/0),
    Ping = spawn(fun() -> ping_loop(Relay, Parent, 0) end),
    Pong = spawn(fun() -> pong_loop(Ping, 0) end),
    Relay ! {target, Pong},
    Start = erlang:monotonic_time(nanosecond),
    Ping ! {kick, ?PING_N},
    Count = receive {ping_done, C} -> C end,
    Elapsed = erlang:monotonic_time(nanosecond) - Start,
    true = (Count =:= ?PING_N),
    Ping ! stop,
    Pong ! stop,
    Relay ! stop,
    report("ping_pong", 2 * ?PING_N + 1, Elapsed).

relay_loop() ->
    receive
        {target, Target} -> relay_loop(Target);
        stop -> ok
    end.
relay_loop(Target) ->
    receive
        Msg ->
            case Msg of
                stop -> ok;
                _ ->
                    Target ! Msg,
                    relay_loop(Target)
            end
    end.

ring_loop(Parent, Next) ->
    receive
        {setup, N} ->
            Parent ! ring_ready,
            ring_loop(Parent, N);
        {token, H, C} when H > 0 ->
            Next ! {token, H - 1, C + 1},
            ring_loop(Parent, Next);
        {token, 0, C} ->
            Parent ! {ring_done, C},
            ring_loop(Parent, Next);
        stop -> ok
    end.

wait_ready(0) -> ok;
wait_ready(N) ->
    receive ring_ready -> wait_ready(N - 1) end.

thread_ring() ->
    Parent = self(),
    Pids = [spawn(fun() -> ring_loop(Parent, undefined) end)
            || _ <- lists:seq(1, ?RING_N)],
    wire_ring(Pids, Pids),
    wait_ready(?RING_N),
    First = hd(Pids),
    Start = erlang:monotonic_time(nanosecond),
    First ! {token, ?HOPS, 0},
    Total = receive {ring_done, C} -> C end,
    Elapsed = erlang:monotonic_time(nanosecond) - Start,
    true = (Total =:= ?HOPS),
    lists:foreach(fun(P) -> P ! stop end, Pids),
    report("thread_ring", ?HOPS, Elapsed).

wire_ring([Last], All) ->
    Last ! {setup, hd(All)};
wire_ring([A, B | Rest], All) ->
    A ! {setup, B},
    wire_ring([B | Rest], All).

sink_loop(Parent, Count) ->
    receive
        ack ->
            Next = Count + 1,
            case Next of
                ?TASKS ->
                    Parent ! {fork_done, Next},
                    sink_loop(Parent, Next);
                _ -> sink_loop(Parent, Next)
            end;
        stop -> ok
    end.

worker_loop(Sink) ->
    receive
        task ->
            Sink ! ack,
            worker_loop(Sink);
        stop -> ok
    end.

send_tasks(_Workers, ?TASKS) -> ok;
send_tasks(Workers, I) ->
    Worker = lists:nth((I rem ?WORKERS) + 1, Workers),
    Worker ! task,
    send_tasks(Workers, I + 1).

fork_join() ->
    Parent = self(),
    Sink = spawn(fun() -> sink_loop(Parent, 0) end),
    Workers = [spawn(fun() -> worker_loop(Sink) end)
               || _ <- lists:seq(1, ?WORKERS)],
    Start = erlang:monotonic_time(nanosecond),
    send_tasks(Workers, 0),
    Count = receive {fork_done, C} -> C end,
    Elapsed = erlang:monotonic_time(nanosecond) - Start,
    true = (Count =:= ?TASKS),
    lists:foreach(fun(P) -> P ! stop end, Workers),
    Sink ! stop,
    report("fork_join", 2 * ?TASKS, Elapsed).

main() ->
    counting(),
    ping_pong(),
    thread_ring(),
    fork_join(),
    ok.
