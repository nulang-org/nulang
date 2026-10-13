#include <caf/all.hpp>
#include <caf/caf_main.hpp>

#include <chrono>
#include <cstdint>
#include <iostream>
#include <memory>
#include <stdexcept>
#include <string>
#include <vector>

using namespace caf;

using inc_atom = atom_constant<atom("inc")>;
using done_atom = atom_constant<atom("done")>;
using kick_atom = atom_constant<atom("kick")>;
using ack_atom = atom_constant<atom("ack")>;
using recv_atom = atom_constant<atom("recv")>;
using setup_atom = atom_constant<atom("setup")>;
using ready_atom = atom_constant<atom("ready")>;
using token_atom = atom_constant<atom("token")>;
using task_atom = atom_constant<atom("task")>;
using stop_atom = atom_constant<atom("stop")>;

constexpr uint64_t count_n = 200'000;
constexpr uint64_t ping_n = 20'000;
constexpr size_t ring_n = 10;
constexpr uint64_t hops = 20'000;
constexpr size_t worker_count = 8;
constexpr uint64_t tasks = 50'000;

uint64_t elapsed_ns(std::chrono::steady_clock::time_point start) {
  return static_cast<uint64_t>(
    std::chrono::duration_cast<std::chrono::nanoseconds>(
      std::chrono::steady_clock::now() - start)
      .count());
}

void report(const char* name, uint64_t messages, uint64_t ns) {
  std::cout << "[cross-bench] runtime=caf benchmark=" << name
            << " messages=" << messages << " elapsed_ns=" << ns << '\n';
}

behavior counter_actor(event_based_actor* self, actor parent) {
  auto count = std::make_shared<uint64_t>(0);
  return {
    [=](inc_atom) mutable {
      ++*count;
      if (*count == count_n)
        self->mail(done_atom::value, *count).send(parent);
    },
    [=](stop_atom) { self->quit(); },
  };
}

behavior pong_actor(event_based_actor* self, actor ping) {
  auto count = std::make_shared<uint64_t>(0);
  return {
    [=](recv_atom) mutable {
      ++*count;
      self->mail(ack_atom::value, *count).send(ping);
    },
    [=](stop_atom) { self->quit(); },
  };
}

behavior ping_actor(event_based_actor* self, actor parent) {
  auto remaining = std::make_shared<uint64_t>(0);
  auto pong = std::make_shared<actor>();
  return {
    [=](kick_atom, uint64_t n, const actor& target) mutable {
      *remaining = n;
      *pong = target;
      self->mail(recv_atom::value).send(*pong);
    },
    [=](ack_atom, uint64_t count) mutable {
      --*remaining;
      if (*remaining == 0)
        self->mail(done_atom::value, count).send(parent);
      else
        self->mail(recv_atom::value).send(*pong);
    },
    [=](stop_atom) { self->quit(); },
  };
}

behavior ring_actor(event_based_actor* self, actor parent) {
  auto next = std::make_shared<actor>();
  return {
    [=](setup_atom, const actor& target) mutable {
      *next = target;
      self->mail(ready_atom::value).send(parent);
    },
    [=](token_atom, uint64_t remaining, uint64_t count) {
      if (remaining > 0)
        self->mail(token_atom::value, remaining - 1, count + 1).send(*next);
      else
        self->mail(done_atom::value, count).send(parent);
    },
    [=](stop_atom) { self->quit(); },
  };
}

behavior sink_actor(event_based_actor* self, actor parent) {
  auto count = std::make_shared<uint64_t>(0);
  return {
    [=](ack_atom) mutable {
      ++*count;
      if (*count == tasks)
        self->mail(done_atom::value, *count).send(parent);
    },
    [=](stop_atom) { self->quit(); },
  };
}

behavior worker_actor(event_based_actor* self, actor sink) {
  return {
    [=](task_atom) { self->mail(ack_atom::value).send(sink); },
    [=](stop_atom) { self->quit(); },
  };
}

void counting(actor_system& sys, scoped_actor& self) {
  auto parent = actor_cast<actor>(self);
  auto counter = sys.spawn(counter_actor, parent);
  auto start = std::chrono::steady_clock::now();
  for (uint64_t i = 0; i < count_n; ++i)
    self->mail(inc_atom::value).send(counter);

  uint64_t count = 0;
  self->receive([&](done_atom, uint64_t value) { count = value; });
  auto elapsed = elapsed_ns(start);
  if (count != count_n)
    throw std::runtime_error("CAF counting lost messages");
  self->mail(stop_atom::value).send(counter);
  report("counting", count_n, elapsed);
}

void ping_pong(actor_system& sys, scoped_actor& self) {
  auto parent = actor_cast<actor>(self);
  auto ping = sys.spawn(ping_actor, parent);
  auto pong = sys.spawn(pong_actor, ping);

  auto start = std::chrono::steady_clock::now();
  self->mail(kick_atom::value, ping_n, pong).send(ping);

  uint64_t count = 0;
  self->receive([&](done_atom, uint64_t value) { count = value; });
  auto elapsed = elapsed_ns(start);
  if (count != ping_n)
    throw std::runtime_error("CAF ping-pong lost messages");

  self->mail(stop_atom::value).send(ping);
  self->mail(stop_atom::value).send(pong);
  report("ping_pong", (2 * ping_n) + 1, elapsed);
}

void thread_ring(actor_system& sys, scoped_actor& self) {
  auto parent = actor_cast<actor>(self);
  std::vector<actor> nodes;
  nodes.reserve(ring_n);
  for (size_t i = 0; i < ring_n; ++i)
    nodes.emplace_back(sys.spawn(ring_actor, parent));

  for (size_t i = 0; i < ring_n; ++i)
    self->mail(setup_atom::value, nodes[(i + 1) % ring_n]).send(nodes[i]);

  size_t ready = 0;
  while (ready < ring_n)
    self->receive([&](ready_atom) { ++ready; });

  auto start = std::chrono::steady_clock::now();
  self->mail(token_atom::value, hops, uint64_t{0}).send(nodes[0]);

  uint64_t total = 0;
  self->receive([&](done_atom, uint64_t value) { total = value; });
  auto elapsed = elapsed_ns(start);
  if (total != hops)
    throw std::runtime_error("CAF thread ring returned wrong hop count");

  for (auto& node : nodes)
    self->mail(stop_atom::value).send(node);
  report("thread_ring", hops, elapsed);
}

void fork_join(actor_system& sys, scoped_actor& self) {
  auto parent = actor_cast<actor>(self);
  auto sink = sys.spawn(sink_actor, parent);

  std::vector<actor> workers;
  workers.reserve(worker_count);
  for (size_t i = 0; i < worker_count; ++i)
    workers.emplace_back(sys.spawn(worker_actor, sink));

  auto start = std::chrono::steady_clock::now();
  for (uint64_t i = 0; i < tasks; ++i)
    self->mail(task_atom::value).send(workers[i % worker_count]);

  uint64_t count = 0;
  self->receive([&](done_atom, uint64_t value) { count = value; });
  auto elapsed = elapsed_ns(start);
  if (count != tasks)
    throw std::runtime_error("CAF fork-join lost tasks");

  for (auto& worker : workers)
    self->mail(stop_atom::value).send(worker);
  self->mail(stop_atom::value).send(sink);
  report("fork_join", 2 * tasks, elapsed);
}

void caf_main(actor_system& sys) {
  scoped_actor self{sys};

  counting(sys, self);
  ping_pong(sys, self);
  thread_ring(sys, self);
  fork_join(sys, self);
}

CAF_MAIN()
