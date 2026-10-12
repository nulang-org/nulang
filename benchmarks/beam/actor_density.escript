#!/usr/bin/env escript
%%! -noshell

%% Same-host comparator for Nulang's manual actor-density probe.
%%
%%   escript benchmarks/beam/actor_density.escript 10000 100000
%%
%% Output:
%% runtime,benchmark,operations,elapsed_us,ops_per_sec

main(Args) ->
    Counts = case Args of
        [] -> [10000, 100000];
        _ -> [list_to_integer(Arg) || Arg <- Args]
    end,
    io:format("runtime,benchmark,operations,elapsed_us,ops_per_sec~n"),
    lists:foreach(fun run_count/1, Counts).

run_count(N) ->
    bench_spawn_idle(N),
    bench_fanout(N).

bench_spawn_idle(N) ->
    Parent = self(),
    {ElapsedUs, Pids} = timer:tc(fun() ->
        [spawn(fun() -> idle_worker(Parent) end) || _ <- lists:seq(1, N)]
    end),
    emit("spawn_idle", N, ElapsedUs),
    lists:foreach(fun(Pid) -> Pid ! stop end, Pids),
    wait_for(stopped, N).

idle_worker(Parent) ->
    receive
        stop -> Parent ! stopped
    end.

bench_fanout(N) ->
    Parent = self(),
    Workers = [spawn(fun() -> fanout_worker(Parent) end) || _ <- lists:seq(1, N)],
    {ElapsedUs, _} = timer:tc(fun() ->
        lists:foreach(fun(Pid) -> Pid ! msg end, Workers),
        wait_for(fanout_ack, N)
    end),
    emit("fanout_one_message_each", N, ElapsedUs).

fanout_worker(Parent) ->
    receive
        msg -> Parent ! fanout_ack
    end.

wait_for(_Tag, 0) ->
    ok;
wait_for(Tag, Remaining) ->
    receive
        Tag -> wait_for(Tag, Remaining - 1)
    end.

emit(Name, Operations, ElapsedUs) ->
    SafeElapsed = erlang:max(ElapsedUs, 1),
    OpsPerSec = (Operations * 1000000) div SafeElapsed,
    io:format("beam,~s,~B,~B,~B~n",
              [Name, Operations, ElapsedUs, OpsPerSec]).
