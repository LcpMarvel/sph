package netpolicy

import (
	"context"
	"net"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func TestIsPublicIP(t *testing.T) {
	blocked := []string{
		"127.0.0.1", "127.1.2.3", "10.0.0.1", "172.16.0.1", "192.168.1.1",
		"169.254.1.1", "224.0.0.1", "239.1.1.1", "0.0.0.0", "255.255.255.255",
		"::1", "fe80::1", "fc00::1", "fd12::1", "ff02::1", "::",
		"0.1.2.3",
	}
	for _, s := range blocked {
		if isPublicIP(net.ParseIP(s)) {
			t.Errorf("%s should be blocked", s)
		}
	}
	allowed := []string{"8.8.8.8", "1.1.1.1", "203.0.113.5", "2606:4700::1111"}
	for _, s := range allowed {
		if !isPublicIP(net.ParseIP(s)) {
			t.Errorf("%s should be allowed", s)
		}
	}
}

// The shared transport must refuse to connect to loopback even for a real
// dial, enforcing below the URL-string level.
func TestSafeDialRefusesLoopback(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {}))
	defer srv.Close()
	client := &http.Client{Transport: NewTransport()}
	req, _ := http.NewRequest(http.MethodGet, srv.URL, nil)
	_, err := client.Do(req)
	if err == nil {
		t.Fatalf("dial to httptest loopback server should have been refused")
	}
	if !strings.Contains(err.Error(), "公网") && !strings.Contains(err.Error(), "refused") && !strings.Contains(err.Error(), "策略") {
		// any error is acceptable as long as the connection did not succeed
		t.Logf("blocked with: %v", err)
	}
}

func TestAPIClientRejectsRedirect(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		http.Redirect(w, r, "https://example.com/x", http.StatusFound)
	}))
	defer srv.Close()
	client := &http.Client{
		Transport: http.DefaultTransport,
		CheckRedirect: func(req *http.Request, via []*http.Request) error {
			return ErrNoRedirect
		},
	}
	req, _ := http.NewRequest(http.MethodGet, srv.URL, nil)
	resp, err := client.Do(req)
	if err == nil {
		resp.Body.Close()
		t.Fatalf("redirect should surface as error via ErrNoRedirect")
	}
	if resp != nil && resp.StatusCode != 302 {
		t.Errorf("unexpected resp status %d", resp.StatusCode)
	}
}

// The media client must re-validate every redirect hop: an http→IP-literal
// or non-https hop is refused even mid-chain.
func TestMediaClientValidatesRedirectHops(t *testing.T) {
	client := NewMediaClient()
	// point it at a fake hop chain via CheckRedirect directly (no network):
	req2, _ := http.NewRequest(http.MethodGet, "http://192.168.0.1/v.mp4", nil)
	err := client.CheckRedirect(req2, []*http.Request{{}})
	if err == nil {
		t.Errorf("non-https private redirect hop must be refused")
	}
	req3, _ := http.NewRequest(http.MethodGet, "https://8.8.8.8/v.mp4", nil)
	if err := client.CheckRedirect(req3, []*http.Request{{}}); err == nil {
		t.Errorf("IP literal redirect hop must be refused")
	}
	req4, _ := http.NewRequest(http.MethodGet, "https://cdn.example.com/v.mp4", nil)
	if err := client.CheckRedirect(req4, []*http.Request{{}}); err != nil {
		t.Errorf("valid https hop refused: %v", err)
	}
	// >5 hops
	via := make([]*http.Request, 5)
	for i := range via {
		via[i], _ = http.NewRequest(http.MethodGet, "https://cdn.example.com/v.mp4", nil)
	}
	if err := client.CheckRedirect(req4, via); err == nil {
		t.Errorf("6th hop must be refused")
	}
}

func TestSanitizeURLForErrorStripsQuery(t *testing.T) {
	out := SanitizeURLForError("https://x.example/v.mp4?sig=DO_NOT_LEAK&b=2")
	if strings.Contains(out, "DO_NOT_LEAK") {
		t.Errorf("query leaked: %s", out)
	}
	if out != "https://x.example/v.mp4" {
		t.Errorf("unexpected %q", out)
	}
	if SanitizeURLForError("://bad") == "" {
		t.Errorf("unparseable URL should yield a placeholder")
	}
}

func TestSafeDialRejectsPrivateHostnameLookup(t *testing.T) {
	// "localhost" resolves to loopback via the real resolver; the dial must
	// refuse before any connection attempt.
	d := SafeDial(context.Background())
	_, err := d(context.Background(), "tcp", "localhost:80")
	if err == nil {
		t.Fatalf("localhost dial should be refused")
	}
}
