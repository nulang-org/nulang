package main

import "fmt"

type message struct {
	report bool
}

func main() {
	const n = 250000
	mailbox := make(chan message, n+1)
	done := make(chan struct{})

	go func() {
		count := 0
		for msg := range mailbox {
			if msg.report {
				fmt.Println(count)
				close(done)
				return
			}
			count++
		}
	}()

	for i := 0; i < n; i++ {
		mailbox <- message{}
	}
	mailbox <- message{report: true}
	<-done
}
