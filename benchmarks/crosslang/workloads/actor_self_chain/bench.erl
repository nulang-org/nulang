-module(bench).
-export([main/0]).

main() ->
    Parent = self(),
    spawn(fun() -> self_chain(250000, 0, Parent) end),
    receive
        {done, Turns} ->
            io:format("~p~n", [Turns])
    end.

self_chain(0, Turns, Parent) ->
    Parent ! {done, Turns};
self_chain(Remaining, Turns, Parent) ->
    self() ! {tick, Remaining - 1},
    receive
        {tick, Next} ->
            self_chain(Next, Turns + 1, Parent)
    end.
