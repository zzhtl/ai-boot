// feishu-go-ws 用官方 Go SDK 收消息，通过 stdin/stdout 交给 ai-boot 处理和回传 ACK。
package main

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"os"
	"os/signal"
	"sync"
	"syscall"
	"time"

	larkcore "github.com/larksuite/oapi-sdk-go/v3/core"
	larkevent "github.com/larksuite/oapi-sdk-go/v3/event"
	"github.com/larksuite/oapi-sdk-go/v3/event/dispatcher"
	"github.com/larksuite/oapi-sdk-go/v3/event/dispatcher/callback"
	larkim "github.com/larksuite/oapi-sdk-go/v3/service/im/v1"
	larkws "github.com/larksuite/oapi-sdk-go/v3/ws"
)

type request struct {
	ID      uint64          `json:"id"`
	Payload json.RawMessage `json:"payload"`
}

type response struct {
	ID      uint64          `json:"id"`
	Payload json.RawMessage `json:"payload"`
	Error   string          `json:"error"`
}

type bridge struct {
	mu      sync.Mutex
	writer  sync.Mutex
	output  *json.Encoder
	nextID  uint64
	pending map[uint64]chan response
}

func newBridge(output io.Writer) *bridge {
	return &bridge{output: json.NewEncoder(output), pending: make(map[uint64]chan response)}
}

func (b *bridge) handle(ctx context.Context, payload []byte) (json.RawMessage, error) {
	ctx, cancel := context.WithTimeout(ctx, 2*time.Second)
	defer cancel()
	b.mu.Lock()
	b.nextID++
	id := b.nextID
	reply := make(chan response, 1)
	b.pending[id] = reply
	b.mu.Unlock()
	defer func() {
		b.mu.Lock()
		delete(b.pending, id)
		b.mu.Unlock()
	}()
	b.writer.Lock()
	err := b.output.Encode(request{ID: id, Payload: payload})
	b.writer.Unlock()
	if err != nil {
		return nil, fmt.Errorf("向 ai-boot 转发事件: %w", err)
	}
	select {
	case result := <-reply:
		if result.Error != "" {
			return nil, errors.New(result.Error)
		}
		return result.Payload, nil
	case <-ctx.Done():
		return nil, fmt.Errorf("等待 ai-boot ACK: %w", ctx.Err())
	}
}

func (b *bridge) readReplies(input io.Reader) error {
	scanner := bufio.NewScanner(input)
	scanner.Buffer(make([]byte, 4096), 8*1024*1024)
	for scanner.Scan() {
		var result response
		if err := json.Unmarshal(scanner.Bytes(), &result); err != nil {
			return fmt.Errorf("解析 ai-boot ACK: %w", err)
		}
		b.mu.Lock()
		reply := b.pending[result.ID]
		b.mu.Unlock()
		if reply != nil {
			select {
			case reply <- result:
			default: // 已收到 ACK，忽略重复响应。
			}
		}
	}
	return scanner.Err()
}

func eventHandler(b *bridge) *dispatcher.EventDispatcher {
	handler := dispatcher.NewEventDispatcher("", "").
		OnP2MessageReceiveV1(func(ctx context.Context, event *larkim.P2MessageReceiveV1) error {
			_, err := b.handle(ctx, event.EventReq.Body)
			return err
		}).
		OnP2CardActionTrigger(func(ctx context.Context, event *callback.CardActionTriggerEvent) (*callback.CardActionTriggerResponse, error) {
			body, err := b.handle(ctx, event.EventReq.Body)
			if err != nil {
				return nil, err
			}
			var result callback.CardActionTriggerResponse
			if len(body) > 0 && string(body) != "null" {
				if err := json.Unmarshal(body, &result); err != nil {
					return nil, fmt.Errorf("解析卡片响应: %w", err)
				}
			}
			return &result, nil
		})
	handler.InitConfig(larkevent.WithLogLevel(larkcore.LogLevelWarn))
	return handler
}

func main() {
	output := os.Stdout
	if len(os.Args) == 2 && os.Args[1] == "--version" {
		if _, err := fmt.Fprintln(output, "feishu-go-ws oapi-sdk-go v3.12.0"); err != nil {
			log.Fatal(err)
		}
		return
	}
	// SDK 的默认日志写 os.Stdout；保留原管道做协议，SDK 日志统一进入 journald。
	os.Stdout = os.Stderr
	// 与 ai-boot 主进程一致，防止同一用户的 Agent 从 /proc 读取 App Secret。
	const prSetDumpable = 4
	if _, _, errno := syscall.Syscall(syscall.SYS_PRCTL, prSetDumpable, 0, 0); errno != 0 {
		log.Fatalf("保护应用凭据失败: %v", errno)
	}
	appID, secret := os.Getenv("FEISHU_APP_ID"), os.Getenv("FEISHU_APP_SECRET")
	if appID == "" || secret == "" {
		log.Fatal("需要 FEISHU_APP_ID 和 FEISHU_APP_SECRET，由 ai-boot 提供")
	}
	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()
	b := newBridge(output)
	go func() {
		if err := b.readReplies(os.Stdin); err != nil {
			log.Printf("ACK 管道关闭: %v", err)
		}
		cancel()
	}()
	handler := eventHandler(b)
	options := []larkws.ClientOption{
		larkws.WithEventHandler(handler), larkws.WithLogLevel(larkcore.LogLevelWarn),
		larkws.WithOnReady(func() { log.Printf("Go SDK 长连接已建立 app_id=%s", appID) }),
		larkws.WithOnReconnected(func() { log.Printf("Go SDK 长连接已重新建立 app_id=%s", appID) }),
	}
	if domain := os.Getenv("FEISHU_BASE_URL"); domain != "" {
		options = append(options, larkws.WithDomain(domain))
	}
	if err := larkws.NewClient(appID, secret, options...).Start(ctx); err != nil && ctx.Err() == nil {
		log.Fatal(err)
	}
}
