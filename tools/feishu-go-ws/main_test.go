package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/larksuite/oapi-sdk-go/v3/event/dispatcher/callback"
)

func TestSDKDispatchPreservesRawEventAndCardResponse(t *testing.T) {
	for _, tc := range []struct{ name, eventType, reply, failure string }{
		{name: "message", eventType: "im.message.receive_v1", reply: "null"},
		{name: "card", eventType: "card.action.trigger", reply: `{"toast":{"type":"info","content":"ok"}}`},
		{name: "failed", eventType: "im.message.receive_v1", reply: "null", failure: "storage failed"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			output, readOutput := io.Pipe()
			input, writeInput := io.Pipe()
			b := newBridge(readOutput)
			defer output.Close()
			defer readOutput.Close()
			defer input.Close()
			defer writeInput.Close()
			readDone := make(chan error, 1)
			go func() { readDone <- b.readReplies(input) }()
			payload := []byte(`{"schema":"2.0","header":{"event_type":"` + tc.eventType + `"},"event":{},"extra":"preserved"}`)
			forwarded := make(chan request, 1)
			go func() {
				var req request
				if err := json.NewDecoder(output).Decode(&req); err != nil {
					readDone <- err
					return
				}
				forwarded <- req
				if err := json.NewEncoder(writeInput).Encode(response{ID: req.ID, Payload: json.RawMessage(tc.reply), Error: tc.failure}); err != nil {
					readDone <- err
				}
			}()
			ctx, cancel := context.WithTimeout(context.Background(), time.Second)
			defer cancel()
			result, err := eventHandler(b).Do(ctx, payload)
			if tc.failure != "" {
				if err == nil || !strings.Contains(err.Error(), tc.failure) {
					t.Fatalf("lost failure: %v", err)
				}
			} else if err != nil {
				t.Fatal(err)
			}
			if tc.name == "card" {
				card, ok := result.(*callback.CardActionTriggerResponse)
				if !ok || card.Toast == nil || card.Toast.Content != "ok" {
					t.Fatalf("lost card response: %#v", result)
				}
			}
			select {
			case req := <-forwarded:
				if !bytes.Equal(req.Payload, payload) {
					t.Fatalf("event changed: %s", req.Payload)
				}
			case <-ctx.Done():
				t.Fatal("event was not forwarded")
			}
			if err := writeInput.Close(); err != nil {
				t.Fatal(err)
			}
			if err := <-readDone; err != nil {
				t.Fatal(err)
			}
		})
	}
}

func TestConcurrentRepliesAreMatchedByID(t *testing.T) {
	requests, output := io.Pipe()
	input, responses := io.Pipe()
	b := newBridge(output)
	defer requests.Close()
	defer output.Close()
	defer input.Close()
	defer responses.Close()
	readDone := make(chan error, 1)
	go func() { readDone <- b.readReplies(input) }()
	done := make(chan error, 1)
	const count = 20
	go func() {
		decoder := json.NewDecoder(requests)
		reqs := make([]request, count)
		for i := range reqs {
			if err := decoder.Decode(&reqs[i]); err != nil {
				done <- err
				return
			}
		}
		// 倒序回 ACK，验证乱序和并发时不会把响应交给其他事件。
		for i := count - 1; i >= 0; i-- {
			if err := json.NewEncoder(responses).Encode(response{ID: reqs[i].ID, Payload: reqs[i].Payload}); err != nil {
				done <- err
				return
			}
		}
		done <- nil
	}()
	var wg sync.WaitGroup
	for i := 0; i < count; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			expected, err := json.Marshal(map[string]int{"index": i})
			if err != nil {
				t.Error(err)
				return
			}
			actual, err := b.handle(context.Background(), expected)
			if err != nil {
				t.Error(err)
				return
			}
			var value map[string]int
			if err := json.Unmarshal(actual, &value); err != nil || value["index"] != i {
				t.Errorf("bad response: %s, %v", actual, err)
			}
		}(i)
	}
	wg.Wait()
	if err := <-done; err != nil {
		t.Fatal(err)
	}
	if err := responses.Close(); err != nil {
		t.Fatal(err)
	}
	if err := <-readDone; err != nil {
		t.Fatal(err)
	}
}

func TestMissingReplyUsesContextDeadlineAndClearsPending(t *testing.T) {
	b := newBridge(io.Discard)
	ctx, cancel := context.WithTimeout(context.Background(), time.Millisecond)
	defer cancel()
	_, err := b.handle(ctx, []byte(`{}`))
	if !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("unexpected error: %v", err)
	}
	if len(b.pending) != 0 {
		t.Fatal("pending request leaked")
	}
}
