package main

import (
	"crypto/hmac"
	"crypto/sha1"
	"encoding/base64"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strconv"
	"strings"
	"testing"
	"time"

	"github.com/gorilla/websocket"
)

// dial opens a WebSocket to the test server and reports the HTTP status the
// handshake ended with: 101 when the upgrade succeeded, or whatever the
// server rejected with (401 for auth failures).
func dial(t *testing.T, srv *httptest.Server, query string) int {
	t.Helper()
	url := "ws" + strings.TrimPrefix(srv.URL, "http") + "/ws?" + query
	conn, resp, err := websocket.DefaultDialer.Dial(url, nil)
	if conn != nil {
		conn.Close()
	}
	if err != nil && resp == nil {
		t.Fatalf("dial failed without an HTTP response: %v", err)
	}
	return resp.StatusCode
}

func TestTokenRequiredWhenConfigured(t *testing.T) {
	h := &hub{token: "s3cret"}
	srv := httptest.NewServer(http.HandlerFunc(h.handleWS))
	defer srv.Close()

	cases := []struct {
		name  string
		query string
		want  int
	}{
		{"missing token", "role=viewer", http.StatusUnauthorized},
		{"wrong token", "role=viewer&token=nope", http.StatusUnauthorized},
		{"correct token viewer", "role=viewer&token=s3cret", http.StatusSwitchingProtocols},
		{"correct token host", "role=host&token=s3cret", http.StatusSwitchingProtocols},
		{"bad role still rejected", "role=admin&token=s3cret", http.StatusBadRequest},
	}
	for _, c := range cases {
		if got := dial(t, srv, c.query); got != c.want {
			t.Errorf("%s: got status %d, want %d", c.name, got, c.want)
		}
	}
}

func TestNoTokenConfiguredIsOpen(t *testing.T) {
	h := &hub{}
	srv := httptest.NewServer(http.HandlerFunc(h.handleWS))
	defer srv.Close()

	if got := dial(t, srv, "role=viewer"); got != http.StatusSwitchingProtocols {
		t.Errorf("got status %d, want %d", got, http.StatusSwitchingProtocols)
	}
}

// A rejected connection must not take over the viewer slot. Auth is checked
// before register(), so an attacker can't displace the real viewer by
// connecting with a bad token.
func TestRejectedConnectionDoesNotDisplaceViewer(t *testing.T) {
	h := &hub{token: "s3cret"}
	srv := httptest.NewServer(http.HandlerFunc(h.handleWS))
	defer srv.Close()

	url := "ws" + strings.TrimPrefix(srv.URL, "http") + "/ws?role=viewer&token=s3cret"
	real, _, err := websocket.DefaultDialer.Dial(url, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer real.Close()

	// Wait for the server to register the real viewer before the attack.
	for i := 0; i < 100; i++ {
		h.mu.Lock()
		registered := h.viewer != nil
		h.mu.Unlock()
		if registered {
			break
		}
	}
	h.mu.Lock()
	before := h.viewer
	h.mu.Unlock()

	dial(t, srv, "role=viewer&token=wrong")

	h.mu.Lock()
	after := h.viewer
	h.mu.Unlock()
	if before == nil || before != after {
		t.Error("a rejected connection changed the registered viewer")
	}
}

// readConfig connects and returns the first message the server sends, which
// must be the ICE config.
func readConfig(t *testing.T, srv *httptest.Server, query string) configMessage {
	t.Helper()
	url := "ws" + strings.TrimPrefix(srv.URL, "http") + "/ws?" + query
	conn, _, err := websocket.DefaultDialer.Dial(url, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	conn.SetReadDeadline(time.Now().Add(2 * time.Second))
	_, data, err := conn.ReadMessage()
	if err != nil {
		t.Fatal(err)
	}
	var cfg configMessage
	if err := json.Unmarshal(data, &cfg); err != nil {
		t.Fatal(err)
	}
	return cfg
}

func TestConfigSentFirstAndEmptyWithoutTurn(t *testing.T) {
	h := &hub{}
	srv := httptest.NewServer(http.HandlerFunc(h.handleWS))
	defer srv.Close()

	for _, role := range []string{"host", "viewer"} {
		cfg := readConfig(t, srv, "role="+role)
		if cfg.Type != "config" {
			t.Errorf("%s: first message type %q, want config", role, cfg.Type)
		}
		if len(cfg.ICEServers) != 0 {
			t.Errorf("%s: got %d ice servers, want none", role, len(cfg.ICEServers))
		}
	}
}

// The credential must equal what coturn computes for use-auth-secret:
// base64(HMAC-SHA1(secret, username)), with the username carrying a future
// expiry timestamp. Recomputed here independently of configMessage.
func TestTurnCredentialMatchesCoturnScheme(t *testing.T) {
	h := &hub{turnHost: "203.0.113.7:3478", turnSecret: "shared-secret"}
	srv := httptest.NewServer(http.HandlerFunc(h.handleWS))
	defer srv.Close()

	cfg := readConfig(t, srv, "role=viewer")
	if len(cfg.ICEServers) != 1 {
		t.Fatalf("got %d ice servers, want 1", len(cfg.ICEServers))
	}
	s := cfg.ICEServers[0]

	mac := hmac.New(sha1.New, []byte("shared-secret"))
	mac.Write([]byte(s.Username))
	want := base64.StdEncoding.EncodeToString(mac.Sum(nil))
	if s.Credential != want {
		t.Errorf("credential %q does not match HMAC-SHA1 of username, want %q", s.Credential, want)
	}

	expiry, err := strconv.ParseInt(strings.SplitN(s.Username, ":", 2)[0], 10, 64)
	if err != nil {
		t.Fatalf("username %q does not start with a unix timestamp", s.Username)
	}
	if expiry <= time.Now().Unix() {
		t.Error("credential is already expired")
	}

	wantURLs := []string{
		"stun:203.0.113.7:3478",
		"turn:203.0.113.7:3478?transport=udp",
		"turn:203.0.113.7:3478?transport=tcp",
	}
	if strings.Join(s.URLs, ",") != strings.Join(wantURLs, ",") {
		t.Errorf("urls %v, want %v", s.URLs, wantURLs)
	}
}
