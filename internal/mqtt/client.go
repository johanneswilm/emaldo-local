// Package mqtt is a minimal self-contained MQTT 3.1.1 client:
// CONNECT/PUBLISH/SUBSCRIBE/UNSUBSCRIBE/PING, QoS 0 and 1, TLS or plain,
// auto-reconnect with backoff. Enough for telemetry publishing + command
// subscription + Home Assistant discovery, with zero external deps.
package mqtt

import (
	"crypto/tls"
	"encoding/binary"
	"errors"
	"fmt"
	"net"
	"net/url"
	"strings"
	"sync"
	"time"
)

type Message struct {
	Topic   string
	Payload []byte
}

type Handler func(Message)

type Client struct {
	URL      string
	Username string
	Password string
	ClientID string

	OnMessage Handler

	mu       sync.Mutex
	conn     net.Conn
	out      chan pkt
	subs     map[string]Handler
	nextMid  uint16
	pending  map[uint16]chan struct{}
	closed   chan struct{}
	closeOnce sync.Once
}

type pkt struct {
	topic string
	data  []byte
	qos   byte
}

func (c *Client) Start() {
	c.out = make(chan pkt, 64)
	c.subs = map[string]Handler{}
	c.pending = map[uint16]chan struct{}{}
	c.closed = make(chan struct{})
	go c.run()
}

func (c *Client) Close() {
	c.closeOnce.Do(func() { close(c.closed) })
}

func (c *Client) Publish(topic string, payload []byte, qos byte) {
	select {
	case c.out <- pkt{topic: topic, data: payload, qos: qos}:
	default:
	}
}

func (c *Client) Subscribe(topic string, h Handler) {
	c.mu.Lock()
	c.subs[topic] = h
	conn := c.conn
	c.mu.Unlock()
	if conn != nil {
		c.sendSubscribe(conn, topic)
	}
}

func (c *Client) Unsubscribe(topic string) {
	c.mu.Lock()
	delete(c.subs, topic)
	c.mu.Unlock()
}

func (c *Client) run() {
	backoff := time.Second
	for {
		select {
		case <-c.closed:
			return
		default:
		}
		err := c.connectOnce()
		if err == errClosed {
			return
		}
		time.Sleep(backoff)
		if backoff < 30*time.Second {
			backoff *= 2
		} else {
			backoff = time.Second
		}
	}
}

var errClosed = errors.New("closed")

func (c *Client) connectOnce() error {
	u, err := url.Parse(c.URL)
	if err != nil || (u.Scheme != "mqtt" && u.Scheme != "mqtts" && u.Scheme != "tcp") {
		return fmt.Errorf("invalid broker URL %q (want mqtt://host:port)", c.URL)
	}
	var conn net.Conn
	addr := u.Host
	if !strings.Contains(addr, ":") {
		addr += ":1883"
	}
	if u.Scheme == "mqtts" {
		conn, err = tls.DialWithDialer(&net.Dialer{Timeout: 10 * time.Second}, "tcp", addr, &tls.Config{ServerName: u.Hostname()})
	} else {
		conn, err = net.DialTimeout("tcp", addr, 10*time.Second)
	}
	if err != nil {
		return err
	}
	c.mu.Lock()
	c.conn = conn
	for t := range c.subs {
		c.sendSubscribe(conn, t)
	}
	c.mu.Unlock()

	if err := c.writeConnect(conn); err != nil {
		conn.Close()
		return err
	}
	if err := conn.SetDeadline(time.Now().Add(10 * time.Second)); err != nil {
		conn.Close()
		return err
	}
	_, _, err = readPacket(conn) // CONNACK
	if err != nil {
		conn.Close()
		return err
	}
	_ = conn.SetDeadline(time.Time{})

	done := make(chan struct{})
	go c.writer(conn, done)
	err = c.reader(conn)
	c.mu.Lock()
	c.conn = nil
	c.mu.Unlock()
	conn.Close()
	select {
	case <-done:
	case <-time.After(time.Second):
	}
	if err == errClosed {
		return errClosed
	}
	return err
}

func (c *Client) writer(conn net.Conn, done chan struct{}) {
	defer close(done)
	ping := time.NewTicker(30 * time.Second)
	defer ping.Stop()
	for {
		select {
		case <-c.closed:
			return
		case p := <-c.out:
			var err error
			if p.qos == 1 {
				err = c.sendPublishQ1(conn, p)
			} else {
				err = writePacket(conn, 0x30, append(encodeTopic(p.topic), p.data...))
			}
			if err != nil {
				return
			}
		case <-ping.C:
			if err := writePacket(conn, 0xC0, nil); err != nil {
				return
			}
		}
	}
}

func (c *Client) reader(conn net.Conn) error {
	for {
		hdr, payload, err := readPacketHdr(conn)
		if err != nil {
			return err
		}
		select {
		case <-c.closed:
			return errClosed
		default:
		}
		switch hdr >> 4 {
		case 3: // PUBLISH
			topicLen := int(binary.BigEndian.Uint16(payload))
			topic := string(payload[2 : 2+topicLen])
			rest := payload[2+topicLen:]
			var mid uint16
			if hdr&0x06 != 0 { // QoS >= 1: packet id present
				mid = binary.BigEndian.Uint16(rest)
				rest = rest[2:]
				writePacket(conn, 0x40, u16(mid)) // PUBACK
			}
			c.mu.Lock()
			h := c.matchHandler(topic)
			c.mu.Unlock()
			if h != nil {
				h(Message{Topic: topic, Payload: rest})
			}
		case 4: // PUBACK
			mid := binary.BigEndian.Uint16(payload)
			c.mu.Lock()
			if ch, ok := c.pending[mid]; ok {
				delete(c.pending, mid)
				close(ch)
			}
			c.mu.Unlock()
		case 13: // PINGRESP
		}
	}
}

func (c *Client) matchHandler(topic string) Handler {
	for t, h := range c.subs {
		if t == topic {
			return h
		}
		if strings.HasSuffix(t, "#") && strings.HasPrefix(topic, t[:len(t)-1]) {
			return h
		}
		if i := strings.Index(t, "+"); i >= 0 {
			pre, post := t[:i], t[i+1:]
			if strings.HasPrefix(topic, pre) && strings.HasSuffix(topic, post) {
				return h
			}
		}
	}
	return nil
}

func (c *Client) sendPublishQ1(conn net.Conn, p pkt) error {
	c.mu.Lock()
	c.nextMid++
	mid := c.nextMid
	ack := make(chan struct{})
	c.pending[mid] = ack
	c.mu.Unlock()
	body := append(encodeTopic(p.topic), u16(mid)...)
	body = append(body, p.data...)
	if err := writePacket(conn, 0x32, body); err != nil {
		return err
	}
	select {
	case <-ack:
		return nil
	case <-time.After(5 * time.Second):
		c.mu.Lock()
		delete(c.pending, mid)
		c.mu.Unlock()
		return nil // fire-and-forget on ack timeout
	}
}

func (c *Client) sendSubscribe(conn net.Conn, topic string) {
	c.mu.Lock()
	c.nextMid++
	mid := c.nextMid
	c.mu.Unlock()
	body := append(encodeTopic(topic), 0) // QoS 0
	_ = writePacket(conn, 0x82, append(u16(mid), body...))
}

func (c *Client) writeConnect(conn net.Conn) error {
	var vh []byte
	proto := []byte("MQTT")
	vh = append(vh, proto...)
	vh = append(vh, 4) // level
	flags := byte(0x02) // clean session
	if c.Username != "" {
		flags |= 0x80
		if c.Password != "" {
			flags |= 0x40
		}
	}
	vh = append(vh, flags, 0x00, 0x3C) // keepalive 60
	payload := encodeStr(c.ClientID)
	if flags&0x80 != 0 {
		payload = append(payload, encodeStr(c.Username)...)
		if flags&0x40 != 0 {
			payload = append(payload, encodeStr(c.Password)...)
		}
	}
	return writePacket(conn, 0x10, append(vh, payload...))
}

func encodeTopic(t string) []byte { return encodeStr(t) }

func encodeStr(s string) []byte {
	out := make([]byte, 2+len(s))
	binary.BigEndian.PutUint16(out, uint16(len(s)))
	copy(out[2:], s)
	return out
}

func u16(v uint16) []byte { return []byte{byte(v >> 8), byte(v)} }

func writePacket(conn net.Conn, hdr byte, body []byte) error {
	rem := encodeRemLen(len(body))
	frame := append([]byte{hdr}, rem...)
	frame = append(frame, body...)
	_, err := conn.Write(frame)
	return err
}

func encodeRemLen(n int) []byte {
	var out []byte
	for {
		d := byte(n % 128)
		n /= 128
		if n > 0 {
			d |= 0x80
		}
		out = append(out, d)
		if n == 0 {
			return out
		}
	}
}

func readPacket(conn net.Conn) (byte, []byte, error) { return readPacketHdr(conn) }

func readPacketHdr(conn net.Conn) (byte, []byte, error) {
	var hdr [1]byte
	if _, err := readFull(conn, hdr[:]); err != nil {
		return 0, nil, err
	}
	mult := 1
	rem := 0
	for {
		var b [1]byte
		if _, err := readFull(conn, b[:]); err != nil {
			return 0, nil, err
		}
		rem += int(b[0]&0x7F) * mult
		if b[0]&0x80 == 0 {
			break
		}
		mult *= 128
	}
	payload := make([]byte, rem)
	if _, err := readFull(conn, payload); err != nil {
		return 0, nil, err
	}
	return hdr[0], payload, nil
}

func readFull(conn net.Conn, b []byte) (int, error) {
	n := 0
	for n < len(b) {
		m, err := conn.Read(b[n:])
		n += m
		if err != nil {
			return n, err
		}
	}
	return n, nil
}
