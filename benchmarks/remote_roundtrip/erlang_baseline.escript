#!/usr/bin/env escript
%%! -noshell +sbwt none +sbwtdcpu none +sbwtdio none

main(Args) ->
    {Roundtrips, Warmup} = parse_args(Args, 10000, 100),
    case Roundtrips > 0 of
        true -> ok;
        false -> io:format(standard_error, "--roundtrips must be at least 1~n", []), halt(2)
    end,

    {ok, Listener} = gen_tcp:listen(0, [binary, {packet, 4}, {active, false},
                                        {nodelay, true}, {reuseaddr, true}]),
    {ok, {_Addr, Port}} = inet:sockname(Listener),
    ServerActor = spawn(fun actor_loop/0),
    ClientActor = spawn(fun actor_loop/0),
    Parent = self(),
    Total = Warmup + Roundtrips,
    spawn(fun() -> accept_and_serve(Listener, ServerActor, Total, Parent) end),

    {ok, Socket} = gen_tcp:connect("127.0.0.1", Port,
                                   [binary, {packet, 4}, {active, false}, {nodelay, true}]),
    warmup(Socket, ClientActor, 1, Warmup),

    Start = erlang:monotonic_time(nanosecond),
    measured(Socket, ClientActor, Warmup + 1, Roundtrips),
    Elapsed = erlang:monotonic_time(nanosecond) - Start,

    receive
        server_done -> ok
    after 5000 ->
        erlang:error(server_timeout)
    end,
    gen_tcp:close(Socket),
    gen_tcp:close(Listener),
    ServerActor ! stop,
    ClientActor ! stop,
    NsPerRoundtrip = Elapsed / Roundtrips,
    io:format(
      "[remote-roundtrip] runtime=erlang iteration=1 roundtrips=~B elapsed_ns=~B ns_per_roundtrip=~.1f~n",
      [Roundtrips, Elapsed, NsPerRoundtrip]).

parse_args([], Roundtrips, Warmup) ->
    {Roundtrips, Warmup};
parse_args(["--roundtrips", Value | Rest], _Roundtrips, Warmup) ->
    parse_args(Rest, list_to_integer(Value), Warmup);
parse_args(["--warmup", Value | Rest], Roundtrips, _Warmup) ->
    parse_args(Rest, Roundtrips, list_to_integer(Value));
parse_args([Unknown | _], _Roundtrips, _Warmup) ->
    io:format(standard_error, "unknown argument: ~s~n", [Unknown]),
    halt(2).

actor_loop() ->
    receive
        {record, Sequence, From, Ref} ->
            From ! {recorded, Ref, Sequence},
            actor_loop();
        stop ->
            ok
    end.

actor_record(Actor, Sequence) ->
    Ref = make_ref(),
    Actor ! {record, Sequence, self(), Ref},
    receive
        {recorded, Ref, Sequence} -> Sequence
    after 2000 ->
        erlang:error(actor_timeout)
    end.

accept_and_serve(Listener, Actor, Count, Parent) ->
    {ok, Socket} = gen_tcp:accept(Listener),
    serve(Socket, Actor, Count),
    gen_tcp:close(Socket),
    Parent ! server_done.

serve(_Socket, _Actor, 0) ->
    ok;
serve(Socket, Actor, Remaining) ->
    {ok, <<Sequence:64/signed-big>>} = gen_tcp:recv(Socket, 0, 2000),
    Sequence = actor_record(Actor, Sequence),
    ok = gen_tcp:send(Socket, <<Sequence:64/signed-big>>),
    serve(Socket, Actor, Remaining - 1).

warmup(_Socket, _Actor, _Sequence, 0) ->
    ok;
warmup(Socket, Actor, Sequence, Remaining) ->
    roundtrip(Socket, Actor, Sequence),
    warmup(Socket, Actor, Sequence + 1, Remaining - 1).

measured(_Socket, _Actor, _Sequence, 0) ->
    ok;
measured(Socket, Actor, Sequence, Remaining) ->
    roundtrip(Socket, Actor, Sequence),
    measured(Socket, Actor, Sequence + 1, Remaining - 1).

roundtrip(Socket, Actor, Sequence) ->
    ok = gen_tcp:send(Socket, <<Sequence:64/signed-big>>),
    {ok, <<Returned:64/signed-big>>} = gen_tcp:recv(Socket, 0, 2000),
    Sequence = Returned,
    Sequence = actor_record(Actor, Returned),
    ok.
