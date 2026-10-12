%% Savina-style workloads using real OTP gen_server actors.
%% Intentionally separate from the raw BEAM-process baseline.
-module(cross_runtime_otp).
-behaviour(gen_server).

-export([main/0, init/1, handle_call/3, handle_cast/2,
         handle_info/2, terminate/2, code_change/3]).

-define(COUNT_N, 200000).
-define(PING_N, 20000).
-define(RING_N, 10).
-define(HOPS, 20000).
-define(WORKERS, 8).
-define(TASKS, 50000).

%% Fixtures use gen_server:cast (including envelope and handler dispatch)
%% for all actor-to-actor and producer-to-actor workload messages.
%% Only completion signals use native Erlang messages back to the harness.

init({counter, Parent}) -> {ok, {counter, Parent, 0}};
init({ping, Parent}) -> {ok, {ping, Parent, undefined, 0}};
init({pong, Ping}) -> {ok, {pong, Ping, 0}};
init({ring, Parent}) -> {ok, {ring, Parent, undefined}};
init({sink, Parent}) -> {ok, {sink, Parent, 0}};
init({worker, Sink}) -> {ok, {worker, Sink}}.

handle_call({set_next, Next}, _From, {ping, Parent, _, Remaining}) ->
    {reply, ok, {ping, Parent, Next, Remaining}};
handle_call({set_next, Next}, _From, {ring, Parent, _}) ->
    {reply, ok, {ring, Parent, Next}};
handle_call(Request, _From, State) ->
    {stop, {unexpected_call, Request}, {error, unexpected_call}, State}.

handle_cast(inc, {counter, Parent, Count}) ->
    Next = Count + 1,
    if Next =:= ?COUNT_N -> Parent ! {count_done, Next};
       true -> ok
    end,
    {noreply, {counter, Parent, Next}};
handle_cast({kick, N}, {ping, Parent, Pong, _Remaining}) ->
    gen_server:cast(Pong, recv),
    {noreply, {ping, Parent, Pong, N}};
handle_cast({ack, PongCount}, {ping, Parent, Pong, Remaining}) when Remaining > 0 ->
    Next = Remaining - 1,
    case Next of
        0 -> Parent ! {ping_done, PongCount};
        _ -> gen_server:cast(Pong, recv)
    end,
    {noreply, {ping, Parent, Pong, Next}};
handle_cast(recv, {pong, Ping, Count}) ->
    Next = Count + 1,
    gen_server:cast(Ping, {ack, Next}),
    {noreply, {pong, Ping, Next}};
handle_cast({token, H, C}, {ring, Parent, Next}) when H > 0 ->
    gen_server:cast(Next, {token, H - 1, C + 1}),
    {noreply, {ring, Parent, Next}};
handle_cast({token, 0, C}, {ring, Parent, Next}) ->
    Parent ! {ring_done, C},
    {noreply, {ring, Parent, Next}};
handle_cast(task, {worker, Sink}) ->
    gen_server:cast(Sink, ack),
    {noreply, {worker, Sink}};
handle_cast(ack, {sink, Parent, Count}) ->
    Next = Count + 1,
    if Next =:= ?TASKS -> Parent ! {fork_done, Next};
       true -> ok
    end,
    {noreply, {sink, Parent, Next}};
handle_cast(Other, State) ->
    {stop, {unexpected_cast, Other}, State}.

handle_info(Info, State) ->
    {stop, {unexpected_info, Info}, State}.
terminate(_Reason, _State) -> ok.
code_change(_OldVsn, State, _Extra) -> {ok, State}.

report(Name, LogicalMessages, ElapsedNs) ->
    io:format("[cross-bench] runtime=erlang_otp benchmark=~s messages=~B elapsed_ns=~B~n",
              [Name, LogicalMessages, ElapsedNs]).

spawn_actor(Args) ->
    {ok, Pid} = gen_server:start_link(?MODULE, Args, []),
    Pid.

send_n(_Pid, 0, _Message) -> ok;
send_n(Pid, N, Message) ->
    gen_server:cast(Pid, Message),
    send_n(Pid, N - 1, Message).

counting() ->
    Parent = self(),
    Counter = spawn_actor({counter, Parent}),
    Start = erlang:monotonic_time(nanosecond),
    send_n(Counter, ?COUNT_N, inc),
    Count = receive {count_done, C} -> C end,
    Elapsed = erlang:monotonic_time(nanosecond) - Start,
    ?COUNT_N = Count,
    ok = gen_server:stop(Counter),
    report("counting", ?COUNT_N, Elapsed).

ping_pong() ->
    Parent = self(),
    Ping = spawn_actor({ping, Parent}),
    Pong = spawn_actor({pong, Ping}),
    ok = gen_server:call(Ping, {set_next, Pong}),
    Start = erlang:monotonic_time(nanosecond),
    gen_server:cast(Ping, {kick, ?PING_N}),
    Count = receive {ping_done, C} -> C end,
    Elapsed = erlang:monotonic_time(nanosecond) - Start,
    ?PING_N = Count,
    ok = gen_server:stop(Ping),
    ok = gen_server:stop(Pong),
    report("ping_pong", 2 * ?PING_N + 1, Elapsed).

wire_ring(Pids) ->
    Rotated = tl(Pids) ++ [hd(Pids)],
    lists:foreach(fun({Pid, Next}) ->
                      ok = gen_server:call(Pid, {set_next, Next})
                  end, lists:zip(Pids, Rotated)).

thread_ring() ->
    Parent = self(),
    Pids = [spawn_actor({ring, Parent}) || _ <- lists:seq(1, ?RING_N)],
    wire_ring(Pids),
    Start = erlang:monotonic_time(nanosecond),
    gen_server:cast(hd(Pids), {token, ?HOPS, 0}),
    Total = receive {ring_done, C} -> C end,
    Elapsed = erlang:monotonic_time(nanosecond) - Start,
    ?HOPS = Total,
    lists:foreach(fun(Pid) -> ok = gen_server:stop(Pid) end, Pids),
    report("thread_ring", ?HOPS, Elapsed).

send_tasks(_Workers, ?TASKS) -> ok;
send_tasks(Workers, I) ->
    Pid = lists:nth((I rem ?WORKERS) + 1, Workers),
    gen_server:cast(Pid, task),
    send_tasks(Workers, I + 1).

fork_join() ->
    Parent = self(),
    Sink = spawn_actor({sink, Parent}),
    Workers = [spawn_actor({worker, Sink}) || _ <- lists:seq(1, ?WORKERS)],
    Start = erlang:monotonic_time(nanosecond),
    send_tasks(Workers, 0),
    Count = receive {fork_done, C} -> C end,
    Elapsed = erlang:monotonic_time(nanosecond) - Start,
    ?TASKS = Count,
    lists:foreach(fun(Pid) -> ok = gen_server:stop(Pid) end, Workers),
    ok = gen_server:stop(Sink),
    report("fork_join", 2 * ?TASKS, Elapsed).

main() ->
    counting(),
    ping_pong(),
    thread_ring(),
    fork_join(),
    ok.
