// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Package web serves the setup wizard, the single-page UI and the REST
// API. Auth: bearer token or session cookie; everything except
// /api/v1/setup and static assets requires auth once the admin password
// is set.
package web

import (
	"crypto/rand"
	"crypto/sha256"
	"crypto/subtle"
	_ "embed"
	"encoding/base64"
	"encoding/json"
	"net/http"
	"strings"
	"time"

	"emaldo-local/internal/config"
	"emaldo-local/internal/telemetry"
)

//go:embed static/index.html
var indexHTML []byte

type Server struct {
	Store *config.Store
	Telem *telemetry.Store
	// OnConfigChange is called after a successful PUT /api/v1/config.
	OnConfigChange func()
	// CmdHandler receives validated MQTT-style command payloads.
	CmdHandler func(cmd string, arg map[string]any)

	sessions map[string]time.Time
}

func New(st *config.Store, tm *telemetry.Store) *Server {
	return &Server{Store: st, Telem: tm, sessions: map[string]time.Time{}}
}

func (s *Server) Handler() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("/", s.root)
	mux.HandleFunc("/api/v1/setup", s.setupAllowed(s.handleSetup))
	mux.HandleFunc("/api/v1/login", s.handleLogin)
	mux.HandleFunc("/api/v1/config", s.authed(s.handleConfig))
	mux.HandleFunc("/api/v1/state", s.authed(s.handleState))
	mux.HandleFunc("/api/v1/events", s.authed(s.handleEvents))
	mux.HandleFunc("/api/v1/cmd", s.authed(s.handleCmd))
	mux.HandleFunc("/api/v1/stream", s.authed(s.handleStream))
	return withCORS(mux)
}

func withCORS(h http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Access-Control-Allow-Origin", "*")
		w.Header().Set("Access-Control-Allow-Headers", "Authorization, Content-Type")
		if r.Method == http.MethodOptions {
			return
		}
		h.ServeHTTP(w, r)
	})
}

func (s *Server) root(w http.ResponseWriter, r *http.Request) {
	if r.URL.Path != "/" {
		http.NotFound(w, r)
		return
	}
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	_, _ = w.Write(indexHTML)
}

// setupAllowed: only allow POST /api/v1/setup while no admin password is
// set (first boot). Everything else falls through to auth.
func (s *Server) setupAllowed(h http.HandlerFunc) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		if s.Store.Get().AdminPasswordHash == "" {
			h(w, r)
			return
		}
		http.Error(w, `{"error":"setup already completed"}`, http.StatusForbidden)
	}
}

func (s *Server) authed(h http.HandlerFunc) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		cfg := s.Store.Get()
		if cfg.AdminPasswordHash == "" {
			w.Header().Set("Content-Type", "application/json")
			_, _ = w.Write([]byte(`{"setup_required":true}`))
			return
		}
		tok := strings.TrimPrefix(r.Header.Get("Authorization"), "Bearer ")
		if tok == "" {
			if c, err := r.Cookie("emaldo_session"); err == nil {
				tok = c.Value
			}
		}
		sessOK := false
		if tok != "" {
			exp, ok := s.sessions[tok]
			sessOK = ok && time.Now().Before(exp)
		}
		if !sessOK {
			w.Header().Set("Content-Type", "application/json")
			w.WriteHeader(http.StatusUnauthorized)
			_, _ = w.Write([]byte(`{"error":"unauthorized"}`))
			return
		}
		h(w, r)
	}
}

func writeJSON(w http.ResponseWriter, v any) {
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(v)
}

func (s *Server) handleSetup(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, "method", http.StatusMethodNotAllowed)
		return
	}
	var req struct {
		Password  string `json:"password"`
		BrokerURL string `json:"broker_url"`
		Username  string `json:"username"`
		PasswordM string `json:"mqtt_password"`
		DeviceName string `json:"device_name"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil || len(req.Password) < 8 {
		http.Error(w, `{"error":"password required (min 8 chars)"}`, http.StatusBadRequest)
		return
	}
	cfg := s.Store.Get()
	salt := make([]byte, 16)
	_, _ = rand.Read(salt)
	cfg.AdminPasswordSalt = base64.StdEncoding.EncodeToString(salt)
	cfg.AdminPasswordHash = hashPassword(req.Password, cfg.AdminPasswordSalt)
	cfg.MQTT.BrokerURL = req.BrokerURL
	cfg.MQTT.Username = req.Username
	cfg.MQTT.Password = req.PasswordM
	cfg.Device.Name = req.DeviceName
	if err := s.Store.Save(cfg); err != nil {
		http.Error(w, `{"error":"save failed"}`, http.StatusInternalServerError)
		return
	}
	if s.OnConfigChange != nil {
		s.OnConfigChange()
	}
	writeJSON(w, map[string]any{"ok": true})
}

func (s *Server) handleLogin(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, "method", http.StatusMethodNotAllowed)
		return
	}
	var req struct {
		Password string `json:"password"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, `{"error":"bad request"}`, http.StatusBadRequest)
		return
	}
	cfg := s.Store.Get()
	if cfg.AdminPasswordHash == "" || hashPassword(req.Password, cfg.AdminPasswordSalt) != cfg.AdminPasswordHash {
		w.WriteHeader(http.StatusUnauthorized)
		writeJSON(w, map[string]any{"error": "invalid password"})
		return
	}
	tokBytes := make([]byte, 24)
	_, _ = rand.Read(tokBytes)
	tok := base64.RawURLEncoding.EncodeToString(tokBytes)
	s.sessions[tok] = time.Now().Add(30 * 24 * time.Hour)
	http.SetCookie(w, &http.Cookie{Name: "emaldo_session", Value: tok, Path: "/", HttpOnly: true, SameSite: http.SameSiteLaxMode})
	writeJSON(w, map[string]any{"token": tok})
}

func hashPassword(pw, saltB64 string) string {
	salt, _ := base64.StdEncoding.DecodeString(saltB64)
	h := sha256.Sum256(append(salt, []byte(pw)...))
	return base64.StdEncoding.EncodeToString(h[:])
}

func (s *Server) handleConfig(w http.ResponseWriter, r *http.Request) {
	switch r.Method {
	case http.MethodGet:
		writeJSON(w, config.MaskSecrets(s.Store.Get()))
	case http.MethodPut:
		var in config.Config
		if err := json.NewDecoder(r.Body).Decode(&in); err != nil {
			http.Error(w, `{"error":"bad json"}`, http.StatusBadRequest)
			return
		}
		cur := s.Store.Get()
		in.AdminPasswordHash = cur.AdminPasswordHash // never via API
		in.AdminPasswordSalt = cur.AdminPasswordSalt
		s.Store.MergeSecrets(&in)
		if err := s.Store.Save(&in); err != nil {
			http.Error(w, `{"error":"save failed"}`, http.StatusInternalServerError)
			return
		}
		if s.OnConfigChange != nil {
			s.OnConfigChange()
		}
		writeJSON(w, config.MaskSecrets(&in))
	default:
		http.Error(w, "method", http.StatusMethodNotAllowed)
	}
}

func (s *Server) handleState(w http.ResponseWriter, r *http.Request) {
	writeJSON(w, s.Telem.Current())
}

func (s *Server) handleEvents(w http.ResponseWriter, r *http.Request) {
	writeJSON(w, s.Telem.Events())
}

func (s *Server) handleCmd(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, "method", http.StatusMethodNotAllowed)
		return
	}
	var req struct {
		Cmd  string         `json:"cmd"`
		Args map[string]any `json:"args"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, `{"error":"bad json"}`, http.StatusBadRequest)
		return
	}
	allowed := map[string]bool{"set_charge_limit": true, "set_discharge_limit": true,
		"set_mode": true, "reboot": true, "rescan": true}
	if !allowed[req.Cmd] {
		http.Error(w, `{"error":"unknown command"}`, http.StatusBadRequest)
		return
	}
	if s.CmdHandler != nil {
		s.CmdHandler(req.Cmd, req.Args)
	}
	s.Telem.AddEvent("info", "api", "cmd "+req.Cmd)
	writeJSON(w, map[string]any{"ok": true})
}

// handleStream: server-sent events of state updates.
func (s *Server) handleStream(w http.ResponseWriter, r *http.Request) {
	fl, ok := w.(http.Flusher)
	if !ok {
		http.Error(w, "no streaming", http.StatusInternalServerError)
		return
	}
	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	ch, unsub := s.Telem.Subscribe()
	defer unsub()
	for {
		select {
		case st := <-ch:
			b, _ := json.Marshal(st)
			_, _ = w.Write([]byte("data: "))
			_, _ = w.Write(b)
			_, _ = w.Write([]byte("\n\n"))
			fl.Flush()
		case <-r.Context().Done():
			return
		}
	}
}

// ConstantTimeCompare helper for tests/tooling.
func CompareToken(a, b string) bool {
	return subtle.ConstantTimeCompare([]byte(a), []byte(b)) == 1
}
