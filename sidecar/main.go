// rlsidecar: tiny localhost HTTP bridge to Rocket League's PsyNet API (via dank/rlapi).
// The Rust tracker spawns this and asks it for current ranks.
//
//	GET  /status  -> {"state":"starting|login|ready|error","verification_uri","user_code","error"}
//	POST /skills  {"player_ids":["Steam|7656...|0", ...]} -> {"players":[{"player_id","name","skills":[...]}]}
package main

import (
	"context"
	"encoding/json"
	"fmt"
	"flag"
	"log"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"github.com/dank/rlapi"
)

type server struct {
	mu     sync.Mutex // serialises upstream requests and reconnects
	egs    *rlapi.EGS
	psynet *rlapi.PsyNet
	eos    *rlapi.EOSTokenResponse
	rpc    *rlapi.PsyNetRPC
	tokenF string
	openLoginPage bool

	stMu   sync.RWMutex
	state  string
	uri    string
	code   string
	errMsg string
}

func (s *server) setState(state, uri, code, errMsg string) {
	s.stMu.Lock()
	s.state, s.uri, s.code, s.errMsg = state, uri, code, errMsg
	s.stMu.Unlock()
}

func (s *server) saveToken() {
	if s.eos == nil || s.eos.RefreshToken == "" {
		return
	}
	_ = os.MkdirAll(filepath.Dir(s.tokenF), 0o700)
	if err := os.WriteFile(s.tokenF, []byte(s.eos.RefreshToken), 0o600); err != nil {
		log.Println("could not save refresh token:", err)
	}
}

func (s *server) connect() error {
	rpc, err := s.psynet.AuthPlayer(s.eos.AccessToken, s.eos.AccountID, "")
	if err != nil {
		return err
	}
	s.rpc = rpc
	return nil
}

// ensure reconnects (refreshing the EOS token) if the socket dropped. Caller holds s.mu.
func (s *server) ensure() error {
	if s.rpc != nil && s.rpc.IsConnected() {
		return nil
	}
	if s.eos.RefreshToken != "" {
		if fresh, err := s.egs.RefreshEOSToken(s.eos.RefreshToken); err == nil {
			s.eos = fresh
			s.saveToken()
		}
	}
	return s.connect()
}

// login tries the saved refresh token first, then falls back to the device-code flow.
func (s *server) login() error {
	if b, err := os.ReadFile(s.tokenF); err == nil && strings.TrimSpace(string(b)) != "" {
		if tok, err := s.egs.RefreshEOSToken(strings.TrimSpace(string(b))); err == nil {
			s.eos = tok
			s.saveToken()
			return nil
		}
		log.Println("saved token rejected; starting fresh login")
	}
	dev, err := s.egs.AuthenticateWithDevice()
	if err != nil {
		return err
	}
	s.setState("login", dev.VerificationURI, dev.UserCode, "")
	if s.openLoginPage {
		openBrowser(dev.VerificationURI + "?userCode=" + dev.UserCode)
		fmt.Printf("Enter code %s if the page does not fill it in.\n", dev.UserCode)
	}
	log.Printf("login: open %s and enter %s", dev.VerificationURI, dev.UserCode)
	tok, err := s.egs.WaitForDeviceAuthorization(dev)
	if err != nil {
		return err
	}
	s.eos = tok
	s.saveToken()
	return nil
}

type skillOut struct {
	Playlist  int     `json:"playlist"`
	MMR       float64 `json:"mmr"`
	Tier      int     `json:"tier"`
	Division  int     `json:"division"`
	Matches   int     `json:"matches"`
	Placement int     `json:"placement_matches"`
}

type playerOut struct {
	PlayerID string     `json:"player_id"`
	Name     string     `json:"name,omitempty"`
	Skills   []skillOut `json:"skills"`
}

func (s *server) handleStatus(w http.ResponseWriter, _ *http.Request) {
	s.stMu.RLock()
	defer s.stMu.RUnlock()
	json.NewEncoder(w).Encode(map[string]string{
		"state": s.state, "verification_uri": s.uri, "user_code": s.code, "error": s.errMsg,
	})
}

func (s *server) handleSkills(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, "use POST", http.StatusMethodNotAllowed)
		return
	}
	var req struct {
		PlayerIDs []string `json:"player_ids"`
	}
	if err := json.NewDecoder(http.MaxBytesReader(w, r.Body, 1<<16)).Decode(&req); err != nil {
		http.Error(w, "bad JSON: "+err.Error(), http.StatusBadRequest)
		return
	}
	var ids []rlapi.PlayerID
	for _, raw := range req.PlayerIDs {
		if _, _, err := rlapi.ParsePlayerID(raw); err == nil {
			ids = append(ids, rlapi.PlayerID(raw))
		}
	}
	if len(ids) == 0 {
		json.NewEncoder(w).Encode(map[string]any{"players": []playerOut{}})
		return
	}

	s.stMu.RLock()
	ready := s.state == "ready"
	s.stMu.RUnlock()
	if !ready {
		http.Error(w, "not logged in yet", http.StatusServiceUnavailable)
		return
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if err := s.ensure(); err != nil {
		http.Error(w, "upstream connection failed: "+err.Error(), http.StatusBadGateway)
		return
	}
	ctx, cancel := context.WithTimeout(r.Context(), 15*time.Second)
	defer cancel()

	skills, err := s.rpc.GetPlayersSkills(ctx, ids)
	if err != nil {
		http.Error(w, "skills: "+err.Error(), http.StatusBadGateway)
		return
	}
	byID := map[string]*playerOut{}
	var out []playerOut
	for _, p := range skills {
		po := playerOut{PlayerID: p.PlayerID.String(), Skills: []skillOut{}}
		for _, sk := range p.Skills {
			po.Skills = append(po.Skills, skillOut{sk.Playlist, sk.MMR, sk.Tier, sk.Division, sk.MatchesPlayed, sk.PlacementMatchesPlayed})
		}
		out = append(out, po)
	}
	for i := range out {
		byID[out[i].PlayerID] = &out[i]
	}
	// Names are best-effort (the in-game name can be censored); ignore failures.
	if profiles, err := s.rpc.GetProfiles(ctx, ids); err == nil {
		for _, pr := range profiles {
			if po, ok := byID[pr.PlayerID]; ok {
				po.Name = pr.PlayerName
			}
		}
	}
	json.NewEncoder(w).Encode(map[string]any{"players": out})
}

func main() {
	addr := flag.String("addr", "127.0.0.1:49200", "listen address")
	tokenFile := flag.String("token-file", "", "where to keep the Epic refresh token")
	loginOnly := flag.Bool("login-only", false, "log in to Epic (opens the browser), save the token, then exit")
	flag.Parse()
	if *tokenFile == "" {
		d, _ := os.UserConfigDir()
		*tokenFile = filepath.Join(d, "RLTracker", "eos_refresh_token")
	}

	s := &server{egs: rlapi.NewEGS(), psynet: rlapi.NewPsyNet(), tokenF: *tokenFile, openLoginPage: *loginOnly}
	s.setState("starting", "", "", "")

	if *loginOnly {
		if err := s.login(); err != nil {
			log.Fatal("login failed: ", err)
		}
		fmt.Println("Login saved. You can start the tracker now.")
		return
	}

	mux := http.NewServeMux()
	mux.HandleFunc("/status", s.handleStatus)
	mux.HandleFunc("/skills", s.handleSkills)

	go func() {
		s.mu.Lock()
		defer s.mu.Unlock()
		if err := s.login(); err != nil {
			s.setState("error", "", "", err.Error())
			return
		}
		if err := s.connect(); err != nil {
			s.setState("error", "", "", err.Error())
			return
		}
		s.setState("ready", "", "", "")
		log.Println("ready")
	}()

	log.Println("listening on", *addr)
	log.Fatal(http.ListenAndServe(*addr, mux))
}

func openBrowser(url string) {
	_ = exec.Command("rundll32", "url.dll,FileProtocolHandler", url).Start()
}
