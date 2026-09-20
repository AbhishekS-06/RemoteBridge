package main

import (
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
}

var upgrader = websocket.Upgrader{
	// Same-LAN dev setup, browser page and Go server on different ports.
	// No origin check for now -- this whole server has no auth until v4,
	// so locking down origin here wouldn't add real protection.
	CheckOrigin: func(r *http.Request) bool { return true },
}

func (h *hub) register(role string, conn *websocket.Conn) {
	h.mu.Lock()
	defer h.mu.Unlock()
	if role == "host" {
		h.host = conn
	} else {
		h.viewer = conn
	}
}

func (h *hub) unregister(role string, conn *websocket.Conn) {
	h.mu.Lock()
	defer h.mu.Unlock()
	if role == "host" && h.host == conn {
		h.host = nil
	}
	if role == "viewer" && h.viewer == conn {
		h.viewer = nil
	}
}

// peerOf returns the *other* slot's connection, so a message from the host
// goes to the viewer and vice versa. Read under lock since the other side
// can disconnect concurrently.
func (h *hub) peerOf(role string) *websocket.Conn {
	h.mu.Lock()
	defer h.mu.Unlock()
	if role == "host" {
		return h.viewer
	}
	return h.host
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
		// Signaling messages are JSON blobs the host and browser agree on
		// (SDP offers/answers, ICE candidates). This server treats them as
		// opaque bytes -- it relays, it doesn't parse -- so it stays correct
		// even if the message shape changes later.
		msgType, msg, err := conn.ReadMessage()
		if err != nil {
			log.Printf("%s disconnected: %v", role, err)
			return
		}

		peer := h.peerOf(role)
		if peer == nil {
			continue // other side isn't connected yet, drop it
		}
		if err := peer.WriteMessage(msgType, msg); err != nil {
			log.Println("relay failed:", err)
		}
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
