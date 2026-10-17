package main

import (
    "encoding/binary"
    "flag"
    "fmt"
    "io"
    "net"
    "os"
    "sync"
    "time"
)

type actorRequest struct {
    sequence int64
    done     chan int64
}

type actor struct {
    inbox chan actorRequest
}

func newActor() *actor {
    a := &actor{inbox: make(chan actorRequest)}
    go func() {
        var seen int64
        for req := range a.inbox {
            seen = req.sequence
            req.done <- seen
        }
    }()
    return a
}

func (a *actor) record(sequence int64) int64 {
    done := make(chan int64, 1)
    a.inbox <- actorRequest{sequence: sequence, done: done}
    return <-done
}

func writeSequence(w io.Writer, sequence int64) error {
    var buf [8]byte
    binary.BigEndian.PutUint64(buf[:], uint64(sequence))
    _, err := w.Write(buf[:])
    return err
}

func readSequence(r io.Reader) (int64, error) {
    var buf [8]byte
    if _, err := io.ReadFull(r, buf[:]); err != nil {
        return 0, err
    }
    return int64(binary.BigEndian.Uint64(buf[:])), nil
}

func main() {
    roundtrips := flag.Uint64("roundtrips", 10000, "measured request+return operations")
    warmup := flag.Uint64("warmup", 100, "untimed warm-up round trips")
    flag.Parse()
    if *roundtrips == 0 {
        fmt.Fprintln(os.Stderr, "--roundtrips must be at least 1")
        os.Exit(2)
    }

    listener, err := net.Listen("tcp", "127.0.0.1:0")
    if err != nil {
        panic(err)
    }
    defer listener.Close()

    serverActor := newActor()
    clientActor := newActor()
    var serverWG sync.WaitGroup
    serverWG.Add(1)
    total := *warmup + *roundtrips

    go func() {
        defer serverWG.Done()
        conn, err := listener.Accept()
        if err != nil {
            panic(err)
        }
        defer conn.Close()
        if tcp, ok := conn.(*net.TCPConn); ok {
            _ = tcp.SetNoDelay(true)
        }
        for i := uint64(0); i < total; i++ {
            sequence, err := readSequence(conn)
            if err != nil {
                panic(err)
            }
            if got := serverActor.record(sequence); got != sequence {
                panic("server actor state mismatch")
            }
            if err := writeSequence(conn, sequence); err != nil {
                panic(err)
            }
        }
    }()

    conn, err := net.Dial("tcp", listener.Addr().String())
    if err != nil {
        panic(err)
    }
    defer conn.Close()
    if tcp, ok := conn.(*net.TCPConn); ok {
        _ = tcp.SetNoDelay(true)
    }

    roundtrip := func(sequence int64) {
        if err := writeSequence(conn, sequence); err != nil {
            panic(err)
        }
        returned, err := readSequence(conn)
        if err != nil {
            panic(err)
        }
        if returned != sequence {
            panic("wire roundtrip sequence mismatch")
        }
        if got := clientActor.record(returned); got != sequence {
            panic("client actor state mismatch")
        }
    }

    for i := uint64(0); i < *warmup; i++ {
        roundtrip(int64(i + 1))
    }

    started := time.Now()
    for i := uint64(0); i < *roundtrips; i++ {
        roundtrip(int64(*warmup + i + 1))
    }
    elapsed := time.Since(started)

    serverWG.Wait()
    elapsedNS := elapsed.Nanoseconds()
    nsPerRoundtrip := float64(elapsedNS) / float64(*roundtrips)
    fmt.Printf(
        "[remote-roundtrip] runtime=go iteration=1 roundtrips=%d elapsed_ns=%d ns_per_roundtrip=%.1f\n",
        *roundtrips,
        elapsedNS,
        nsPerRoundtrip,
    )
}
