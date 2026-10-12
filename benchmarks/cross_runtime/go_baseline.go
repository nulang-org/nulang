package main

import (
	"fmt"
	"sync"
	"time"
)

const (
	countN  = 200000
	pingN   = 20000
	ringN   = 10
	hops    = 20000
	workers = 8
	tasks   = 50000
)

func report(name string, messages int64, elapsed time.Duration) {
	fmt.Printf("[cross-bench] runtime=go benchmark=%s messages=%d elapsed_ns=%d\n",
		name, messages, elapsed.Nanoseconds())
}

func counting() {
	mailbox := make(chan struct{}, countN)
	done := make(chan int, 1)
	go func() {
		count := 0
		for count < countN {
			<-mailbox
			count++
		}
		done <- count
	}()

	start := time.Now()
	for i := 0; i < countN; i++ {
		mailbox <- struct{}{}
	}
	count := <-done
	elapsed := time.Since(start)
	if count != countN {
		panic("counting lost messages")
	}
	report("counting", countN, elapsed)
}

type pingMsg struct {
	kind  int
	value int
}
type pongMsg struct{ stop bool }

func pingPong() {
	pingBox := make(chan pingMsg, 1)
	pongBox := make(chan pongMsg, 1)
	done := make(chan int, 1)
	var wg sync.WaitGroup
	wg.Add(2)

	go func() {
		defer wg.Done()
		remaining := 0
		for msg := range pingBox {
			switch msg.kind {
			case 0: // kick
				remaining = msg.value
				pongBox <- pongMsg{}
			case 1: // ack
				remaining--
				if remaining == 0 {
					done <- msg.value
				} else {
					pongBox <- pongMsg{}
				}
			case 2:
				return
			}
		}
	}()

	go func() {
		defer wg.Done()
		count := 0
		for msg := range pongBox {
			if msg.stop {
				return
			}
			count++
			pingBox <- pingMsg{kind: 1, value: count}
		}
	}()

	start := time.Now()
	pingBox <- pingMsg{kind: 0, value: pingN}
	count := <-done
	elapsed := time.Since(start)
	if count != pingN {
		panic("ping-pong lost messages")
	}

	pingBox <- pingMsg{kind: 2}
	pongBox <- pongMsg{stop: true}
	wg.Wait()
	report("ping_pong", 2*pingN+1, elapsed)
}

type ringMsg struct {
	h, c int
	stop bool
}

func threadRing() {
	boxes := make([]chan ringMsg, ringN)
	for i := range boxes {
		boxes[i] = make(chan ringMsg, 1)
	}
	done := make(chan int, 1)
	var wg sync.WaitGroup
	wg.Add(ringN)
	for i := 0; i < ringN; i++ {
		rx := boxes[i]
		next := boxes[(i+1)%ringN]
		go func() {
			defer wg.Done()
			for msg := range rx {
				if msg.stop {
					return
				}
				if msg.h > 0 {
					next <- ringMsg{h: msg.h - 1, c: msg.c + 1}
				} else {
					done <- msg.c
				}
			}
		}()
	}

	start := time.Now()
	boxes[0] <- ringMsg{h: hops}
	total := <-done
	elapsed := time.Since(start)
	if total != hops {
		panic("thread ring returned wrong hop count")
	}
	for _, box := range boxes {
		box <- ringMsg{stop: true}
	}
	wg.Wait()
	report("thread_ring", hops, elapsed)
}

type workerMsg struct{ stop bool }

func forkJoin() {
	sink := make(chan struct{}, tasks)
	done := make(chan int, 1)
	go func() {
		count := 0
		for count < tasks {
			<-sink
			count++
		}
		done <- count
	}()

	boxes := make([]chan workerMsg, workers)
	var wg sync.WaitGroup
	wg.Add(workers)
	for i := range boxes {
		boxes[i] = make(chan workerMsg, tasks/workers+1)
		rx := boxes[i]
		go func() {
			defer wg.Done()
			for msg := range rx {
				if msg.stop {
					return
				}
				sink <- struct{}{}
			}
		}()
	}

	start := time.Now()
	for i := 0; i < tasks; i++ {
		boxes[i%workers] <- workerMsg{}
	}
	count := <-done
	elapsed := time.Since(start)
	if count != tasks {
		panic("fork-join lost tasks")
	}
	for _, box := range boxes {
		box <- workerMsg{stop: true}
	}
	wg.Wait()
	report("fork_join", 2*tasks, elapsed)
}

func main() {
	counting()
	pingPong()
	threadRing()
	forkJoin()
}
