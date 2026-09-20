package main

import (
	"encoding/json"
	"log"
	"net/http"
	"sync"

	"github.com/gorilla/websocket"
)

// Two named slots instead of a generic list of peers. v1 has exactly one
// Mac and one phone; a room/session concept only matters once v4 needs
// multiple devices, so it's left out on purpose.
type hub struct {
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
	// Same-LAN dev setup, browser page and Go server on different ports.
	// No origin check for now -- this whole server has no auth until v4,
	// so locking down origin here wouldn't add real protection.
	CheckOrigin: func(r *http.Request) bool { return true },
}

// register fills the role's slot. A viewer that arrives while an offer is
// waiting gets it immediately, so connect order between host and viewer
// no longer matters. Runs under the lock so the replay can't interleave
// with a live relay write to the same connection (gorilla/websocket allows
// only one concurrent writer per connection).
func (h *hub) register(role string, conn *websocket.Conn) {
	h.mu.Lock()
	defer h.mu.Unlock()
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
	h := &hub{}
	http.Handle("/", http.FileServer(http.Dir("../client")))
	http.HandleFunc("/ws", h.handleWS)

	addr := ":8080"
	log.Println("signaling server listening on", addr)
	if err := http.ListenAndServe(addr, nil); err != nil {
		log.Fatal(err)
	}
}
