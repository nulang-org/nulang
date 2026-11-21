-module(bench).
-export([main/0]).

main() ->
    Parent = self(),
    Workers = spawn_workers(64, Parent, []),
    send_work_all(Workers, 2000),
    Total = collect_results(64, 0),
    io:format("~p~n", [Total]).

spawn_workers(0, _Parent, Acc) ->
    Acc;
spawn_workers(N, Parent, Acc) ->
    Pid = spawn(fun() -> worker(0, Parent) end),
    spawn_workers(N - 1, Parent, [Pid | Acc]).

send_work_all([], _Messages) ->
    ok;
send_work_all([Worker | Rest], Messages) ->
    send_work(Worker, Messages),
    Worker ! finish,
    send_work_all(Rest, Messages).

send_work(_Worker, 0) ->
    ok;
send_work(Worker, N) ->
    Worker ! work,
    send_work(Worker, N - 1).

worker(Count, Parent) ->
    receive
        work ->
            worker(Count + 1, Parent);
        finish ->
            Parent ! {done, Count}
    end.

collect_results(0, Total) ->
    Total;
collect_results(Remaining, Total) ->
    receive
        {done, Count} ->
            collect_results(Remaining - 1, Total + Count)
    end.
