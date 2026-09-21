#!/usr/bin/env escript
%%! -noshell

%% Minimal BEAM-side comparator for Nulang's `beam_parity/*` Criterion groups.
%% Run on the same host as the Nulang benchmarks:
%%
%%   escript benchmarks/beam/beam_parity.escript
%%   escript benchmarks/beam/beam_parity.escript 1000 10000 100000
%%
%% Output is CSV so results can be archived or compared without parsing prose:
%% runtime,benchmark,operations,elapsed_us,ops_per_sec

main(Args) ->
    Counts = case Args of
        [] -> [1000, 10000];
        _ -> [list_to_integer(Arg) || Arg <- Args]
    end,
    io:format("runtime,benchmark,operations,elapsed_us,ops_per_sec~n"),
    lists:foreach(fun run_count/1, Counts).

run_count(N) ->
    bench_spawn_idle(N),
    bench_single_mailbox_flood(N),
    bench_fanout_one_message_each(N).

bench_spawn_idle(N) ->
    Parent = self(),
    {ElapsedUs, Pids} = timer:tc(fun() ->
        [spawn(fun() -> idle_worker(Parent) end) || _ <- lists:seq(1, N)]
    end),
    emit("spawn_idle", N, ElapsedUs),
    %% Cleanup is intentionally outside the timed region because the Nulang
    %% benchmark measures actor creation, not termination.
    lists:foreach(fun(Pid) -> Pid ! stop end, Pids),
    wait_for(stopped, N).

idle_worker(Parent) ->
    receive
        stop -> Parent ! stopped
    end.

bench_single_mailbox_flood(N) ->
    Parent = self(),
    Consumer = spawn(fun() -> mailbox_consumer(N, Parent) end),
    {ElapsedUs, _} = timer:tc(fun() ->
        lists:foreach(fun(_) -> Consumer ! msg end, lists:seq(1, N)),
        receive mailbox_done -> ok end
    end),
    emit("single_mailbox_flood", N, ElapsedUs).

mailbox_consumer(0, Parent) ->
    Parent ! mailbox_done;
mailbox_consumer(Remaining, Parent) ->
    receive
        msg -> mailbox_consumer(Remaining - 1, Parent)
    end.

bench_fanout_one_message_each(N) ->
    Parent = self(),
    Workers = [spawn(fun() -> fanout_worker(Parent) end) || _ <- lists:seq(1, N)],
    %% Worker creation is setup, matching the Nulang Criterion benchmark.
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
    io:format(
        "beam,~s,~B,~B,~B~n",
        [Name, Operations, ElapsedUs, OpsPerSec]
    ).
