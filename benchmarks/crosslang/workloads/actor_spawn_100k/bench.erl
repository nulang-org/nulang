-module(bench).
-export([main/0]).

main() ->
    spawn_idle(100000),
    io:format("~p~n", [100000]).

spawn_idle(0) ->
    ok;
spawn_idle(N) ->
    spawn(fun idle/0),
    spawn_idle(N - 1).

idle() ->
    receive
        stop -> ok
    end.
