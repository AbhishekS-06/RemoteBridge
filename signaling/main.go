package main

import (
	"crypto/hmac"
	"crypto/sha1"
	"crypto/subtle"
	"encoding/base64"
	"encoding/json"
	"log"
	"net/http"
	"os"
	"strconv"
	"sync"
	"time"

	"github.com/gorilla/websocket"
)

// How long a minted TURN credential stays valid. coturn checks the expiry
// when a relay allocation is created or refreshed, so a session that
// outlives this needs new credentials on its next allocation.
const turnCredentialTTL = 24 * time.Hour

// Two named slots instead of a generic list of peers. v1 has exactly one
// Mac and one phone; a room/session concept only matters once v4 needs
// multiple devices, so it's left out on purpose.
type hub struct {
	// Shared secret both the host and the viewer must present on connect.
	// Empty means auth is off, which is only sensible for LAN development.
	token string

	// coturn address ("host:port") and the secret it shares with this
	// server. Both empty means no relay is offered and peers use STUN only.
	turnHost   string
	turnSecret string

	mu     sync.Mutex
	host   *websocket.Conn
	viewer *websocket.Conn

	// The host's offer, held until a viewer answers it. The host sends its
	// offer once right after connecting, so without this a viewer that shows
	// up later (or a phone on a slow network) would never see it and the
	// host would wait forever.
	pendingOffer []byte
}

var upgrader = websocket.Upgrader{
	// Origin is not checked. The shared token (see authorized) is the gate:
	// a page on another origin can't connect without knowing it, and origin
	// headers are trivially forged by non-browser clients anyway.
	CheckOrigin: func(r *http.Request) bool { return true },
}

// authorized checks the ?token= query parameter against the configured
// secret. ConstantTimeCompare avoids leaking how many leading characters of
// a guess were right through response timing. With no token configured the
// server is open, which main() warns about at startup.
func (h *hub) authorized(r *http.Request) bool {
	if h.token == "" {
		return true
	}
	got := r.URL.Query().Get("token")
	return subtle.ConstantTimeCompare([]byte(got), []byte(h.token)) == 1
}

type iceServer struct {
	URLs       []string `json:"urls"`
	Username   string   `json:"username,omitempty"`
	Credential string   `json:"credential,omitempty"`
}

type configMessage struct {
	Type       string      `json:"type"`
	ICEServers []iceServer `json:"ice_servers"`
}

// configMessage builds the first message every client receives: the relay
// servers it should give to its peer connection. The browser can't hold a
// permanent TURN password (anyone could read it from the page), so this
// mints a temporary one using coturn's shared-secret scheme: the username is
// "<expiry unix time>:<label>" and the credential is the base64 HMAC-SHA1 of
// that username under the secret both this server and coturn know. coturn
// recomputes the HMAC and rejects it once the expiry has passed. SHA-1 is
// what coturn's protocol specifies; it is not a choice made here.
// Always sends a config message, with an empty list when no relay is set up,
// so clients can wait for it unconditionally instead of guessing.
func (h *hub) configMessage() []byte {
	servers := []iceServer{}
	if h.turnHost != "" && h.turnSecret != "" {
		username := strconv.FormatInt(time.Now().Add(turnCredentialTTL).Unix(), 10) + ":remotebridge"
		mac := hmac.New(sha1.New, []byte(h.turnSecret))
		mac.Write([]byte(username))
		servers = append(servers, iceServer{
			URLs: []string{
				"stun:" + h.turnHost,
				"turn:" + h.turnHost + "?transport=udp",
				"turn:" + h.turnHost + "?transport=tcp",
			},
			Username:   username,
			Credential: base64.StdEncoding.EncodeToString(mac.Sum(nil)),
		})
	}
	msg, _ := json.Marshal(configMessage{Type: "config", ICEServers: servers})
	return msg
}

// register fills the role's slot. A viewer that arrives while an offer is
// waiting gets it immediately, so connect order between host and viewer
// no longer matters. Runs under the lock so the replay can't interleave
// with a live relay write to the same connection (gorilla/websocket allows
// only one concurrent writer per connection).
//
// The config message goes out first, before any offer replay, so a viewer
// always knows its ICE servers before it sees the offer.
func (h *hub) register(role string, conn *websocket.Conn) {
	h.mu.Lock()
	defer h.mu.Unlock()
	if err := conn.WriteMessage(websocket.TextMessage, h.configMessage()); err != nil {
		log.Println("config send failed:", err)
	}
	if role == "host" {
		h.host = conn
		return
	}
	h.viewer = conn
	if h.pendingOffer != nil {
		if err := conn.WriteMessage(websocket.TextMessage, h.pendingOffer); err != nil {
			log.Println("offer replay failed:", err)
		}
	}
}

func (h *hub) unregister(role string, conn *websocket.Conn) {
	h.mu.Lock()
	defer h.mu.Unlock()
	if role == "host" && h.host == conn {
		h.host = nil
		// The offer belongs to this host's peer connection. Once the host
		// is gone, replaying it to a new viewer would hand it a dead offer.
		h.pendingOffer = nil
	}
	if role == "viewer" && h.viewer == conn {
		h.viewer = nil
	}
}

// relay forwards a message to the other role. The server is still a relay,
// not a signaling protocol implementation: the only thing it reads is the
// "type" field, to know when an offer is waiting (remember it) and when it
// has been answered (forget it). Everything else passes through as opaque
// bytes. The whole thing runs under the lock so writes to a connection are
// serialized with register's replay.
func (h *hub) relay(role string, msgType int, msg []byte) {
	var envelope struct {
		Type string `json:"type"`
	}
	_ = json.Unmarshal(msg, &envelope)

	h.mu.Lock()
	defer h.mu.Unlock()

	if role == "host" && envelope.Type == "offer" {
		h.pendingOffer = msg
	}
	if role == "viewer" && envelope.Type == "answer" {
		h.pendingOffer = nil
	}

	peer := h.viewer
	if role == "viewer" {
		peer = h.host
	}
	if peer == nil {
		return // other side isn't connected yet, drop it
	}
	if err := peer.WriteMessage(msgType, msg); err != nil {
		log.Println("relay failed:", err)
	}
}

func (h *hub) handleWS(w http.ResponseWriter, r *http.Request) {
	role := r.URL.Query().Get("role")
	if role != "host" && role != "viewer" {
		http.Error(w, "role must be host or viewer", http.StatusBadRequest)
		return
	}
	// Checked before the upgrade, so a wrong token gets a plain 401 and
	// never reaches register(), where it could displace a real connection.
	if !h.authorized(r) {
		log.Printf("rejected %s connection: bad or missing token", role)
		http.Error(w, "unauthorized", http.StatusUnauthorized)
		return
	}

	conn, err := upgrader.Upgrade(w, r, nil)
	if err != nil {
		log.Println("upgrade failed:", err)
		return
	}
	defer conn.Close()

	h.register(role, conn)
	defer h.unregister(role, conn)
	log.Printf("%s connected", role)

	for {
		msgType, msg, err := conn.ReadMessage()
		if err != nil {
			log.Printf("%s disconnected: %v", role, err)
			return
		}
		h.relay(role, msgType, msg)
	}
}

func main() {
	// Config comes from the environment so the same binary runs on a laptop
	// and on a VPS without editing code.
	h := &hub{
		token:      os.Getenv("REMOTEBRIDGE_TOKEN"),
		turnHost:   os.Getenv("REMOTEBRIDGE_TURN_HOST"),
		turnSecret: os.Getenv("REMOTEBRIDGE_TURN_SECRET"),
	}
	if h.token == "" {
		log.Println("WARNING: REMOTEBRIDGE_TOKEN is not set; anyone who can reach this port can connect")
	}
	switch {
	case h.turnHost != "" && h.turnSecret != "":
		log.Println("TURN relay offered at", h.turnHost)
	case h.turnHost != "" || h.turnSecret != "":
		log.Fatal("set both REMOTEBRIDGE_TURN_HOST and REMOTEBRIDGE_TURN_SECRET, or neither")
	default:
		log.Println("no TURN relay configured; peers will use STUN only")
	}
	http.Handle("/", http.FileServer(http.Dir("../client")))
	http.HandleFunc("/ws", h.handleWS)

	addr := os.Getenv("REMOTEBRIDGE_ADDR")
	if addr == "" {
		addr = ":8080"
	}
	log.Println("signaling server listening on", addr)
	if err := http.ListenAndServe(addr, nil); err != nil {
		log.Fatal(err)
	}
}
