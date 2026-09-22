-module(bench).
-export([main/0]).

main() ->
    Parent = self(),
    Counter = spawn(fun() -> counter(0, Parent) end),
    send_increments(Counter, 250000),
    Counter ! report,
    receive
        done -> ok
    end.

send_increments(_Counter, 0) ->
    ok;
send_increments(Counter, N) ->
    Counter ! inc,
    send_increments(Counter, N - 1).

counter(Count, Parent) ->
    receive
        inc ->
            counter(Count + 1, Parent);
        report ->
            io:format("~p~n", [Count]),
            Parent ! done
    end.
