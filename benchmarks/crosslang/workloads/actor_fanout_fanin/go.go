package main

import "fmt"

type workerMessage struct {
	finish bool
}

func worker(mailbox <-chan workerMessage, results chan<- int) {
	count := 0
	for msg := range mailbox {
		if msg.finish {
			results <- count
			return
		}
		count++
	}
}

func main() {
	const workers = 64
	const messagesPerWorker = 2000

	results := make(chan int, workers)
	mailboxes := make([]chan workerMessage, workers)

	for w := 0; w < workers; w++ {
		mailboxes[w] = make(chan workerMessage, messagesPerWorker+1)
		go worker(mailboxes[w], results)
	}

	for w := 0; w < workers; w++ {
		for i := 0; i < messagesPerWorker; i++ {
			mailboxes[w] <- workerMessage{}
		}
		mailboxes[w] <- workerMessage{finish: true}
	}

	total := 0
	for w := 0; w < workers; w++ {
		total += <-results
	}
	fmt.Println(total)
}
