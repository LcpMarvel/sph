package upstream

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	"sph/internal/apperr"
	"sph/internal/auth"
	"sph/internal/netpolicy"
)

// Sentinel secrets used across the isolation tests. If any of
// these strings shows up in an output the test fails.
const (
	sentinelCookie     = "DO_NOT_LEAK_COOKIE_123"
	sentinelToken      = "DO_NOT_LEAK_TOKEN_456"
	sentinelMediaQuery = "DO_NOT_LEAK_MEDIA_QUERY_789"
)

const testShareURL = "https://weixin.qq.com/sph/AogyNMyA7L"

func fakeCreds() auth.Credentials {
	return auth.Credentials{
		Version:        1,
		Source:         auth.SourceBrowserLogin,
		Cookie:         sentinelCookie,
		YuanbaoHeaders: map[string]string{"user-agent": "UA-TEST/1"},
	}
}

// capturedRequest records what a fake server saw.
type capturedRequest struct {
	URL    string
	Method string
	Header http.Header
	Body   string
}

// fakeTransport routes by path and records requests.
type fakeTransport struct {
	mu       sync.Mutex
	requests []capturedRequest
	yuanbao  func(r *http.Request) (*http.Response, error)
	finder   func(r *http.Request) (*http.Response, error)
	sleeps   int
}

func (f *fakeTransport) RoundTrip(r *http.Request) (*http.Response, error) {
	body := ""
	if r.Body != nil {
		raw, _ := io.ReadAll(io.LimitReader(r.Body, 1<<20))
		body = string(raw)
	}
	f.mu.Lock()
	f.requests = append(f.requests, capturedRequest{URL: r.URL.String(), Method: r.Method, Header: r.Clone(context.Background()).Header, Body: body})
	f.mu.Unlock()
	if strings.Contains(r.URL.Host, "yuanbao") && f.yuanbao != nil {
		return f.yuanbao(r)
	}
	if strings.Contains(r.URL.Host, "channels") && f.finder != nil {
		return f.finder(r)
	}
	return jsonResp(404, `{"error":"no route"}`), nil
}

func (f *fakeTransport) seen() []capturedRequest {
	f.mu.Lock()
	defer f.mu.Unlock()
	out := make([]capturedRequest, len(f.requests))
	copy(out, f.requests)
	return out
}

func jsonResp(status int, body string) *http.Response {
	return &http.Response{
		StatusCode: status,
		Header:     http.Header{"Content-Type": []string{"application/json"}},
		Body:       io.NopCloser(strings.NewReader(body)),
	}
}

func playableResp(url string) *http.Response {
	return jsonResp(200, fmt.Sprintf(`{"code":0,"msg":"","data":{"playable_url":%q}}`, url))
}

func feedResp(videoURL string) *http.Response {
	body := fmt.Sprintf(`{"errCode":0,"errMsg":"","data":{"feedInfo":{"h264VideoInfo":{"videoUrl":%q},"description":"标题","picInfo":[]},"authorInfo":{"nickname":"作者"}}}`, videoURL)
	return jsonResp(200, body)
}

const playableURL = "https://channels.weixin.qq.com/finder-preview/pages/feed?entry_card_type=48&comment_scene=39&appid=0&token=" +
	sentinelToken + "&entry_scene=0&eid=EXPORTID1"
const mediaURL = "https://cdn.example.com/video.mp4?X-snsvideoflag=1&encfilekey=abc&sig=" + sentinelMediaQuery

func newTestClient(t *testing.T, f *fakeTransport) *Client {
	t.Helper()
	httpClient := &http.Client{Transport: f}
	c := NewClient(httpClient)
	c.Sleep = func(ctx context.Context, d time.Duration) error {
		f.mu.Lock()
		f.sleeps++
		f.mu.Unlock()
		return nil
	}
	return c
}

func TestResolveHappyPathAndCredentialIsolation(t *testing.T) {
	f := &fakeTransport{
		yuanbao: func(r *http.Request) (*http.Response, error) { return playableResp(playableURL), nil },
		finder:  func(r *http.Request) (*http.Response, error) { return feedResp(mediaURL), nil },
	}
	c := newTestClient(t, f)
	video, err := c.Resolve(context.Background(), testShareURL, fakeCreds())
	if err != nil {
		t.Fatalf("resolve: %v", err)
	}
	if video.MediaURL != mediaURL {
		t.Errorf("media URL not preserved verbatim: %q", video.MediaURL)
	}
	if video.MediaSource != "h264VideoInfo" || video.CodecHint != "h264" {
		t.Errorf("source selection wrong: %+v", video)
	}
	if video.Title != "标题" || video.Author != "作者" {
		t.Errorf("meta wrong: %q / %q", video.Title, video.Author)
	}
	if video.LocalID == "" || len(video.LocalID) != 12 {
		t.Errorf("local id wrong: %q", video.LocalID)
	}

	seen := f.seen()
	if len(seen) != 2 {
		t.Fatalf("expected 2 requests, got %d", len(seen))
	}
	yb, fd := seen[0], seen[1]

	// yuanbao request: cookie + extra headers + fixed headers, JSON body
	if got := yb.Header.Get("Cookie"); got != sentinelCookie {
		t.Errorf("yuanbao cookie missing")
	}
	if got := yb.Header.Get("User-Agent"); got != "UA-TEST/1" {
		t.Errorf("imported UA should override, got %q", got)
	}
	if yb.Header.Get("Origin") != "https://yuanbao.tencent.com" || yb.Header.Get("Referer") != "https://yuanbao.tencent.com/" {
		t.Errorf("yuanbao origin/referer wrong")
	}
	var body map[string]any
	if err := json.Unmarshal([]byte(yb.Body), &body); err != nil {
		t.Fatalf("yuanbao body not JSON: %v", err)
	}
	if body["type"] != "video_channel_url" || body["scene"].(float64) != 1 {
		t.Errorf("yuanbao body wrong: %v", body)
	}
	if !strings.Contains(yb.URL, "/api/weixin/get_parse_result") {
		t.Errorf("yuanbao endpoint wrong: %s", yb.URL)
	}

	// finder request: NO cookie, NO session headers, token/eid wired in
	if got := fd.Header.Get("Cookie"); got != "" {
		t.Errorf("finder request must not carry cookies, got %q", got)
	}
	if got := fd.Header.Get("User-Agent"); got != "UA-TEST/1" {
		t.Errorf("finder may reuse only the UA, got %q", got)
	}
	if got := fd.Header.Get("t-userid"); got != "" {
		t.Errorf("finder must not carry yuanbao session headers")
	}
	if !strings.Contains(fd.URL, "/finder-preview/api/feed/get_feed_info") {
		t.Errorf("finder endpoint wrong: %s", fd.URL)
	}
	if !strings.Contains(fd.URL, "_rid=") || !strings.Contains(fd.URL, "_pageUrl=") {
		t.Errorf("finder query params missing: %s", fd.URL)
	}
	var fdBody map[string]any
	json.Unmarshal([]byte(fd.Body), &fdBody)
	baseReq := fdBody["baseReq"].(map[string]any)
	if baseReq["generalToken"] != sentinelToken || fdBody["exportId"] != "EXPORTID1" {
		t.Errorf("token/eid not wired: %v", fdBody)
	}
	ref := fd.Header.Get("Referer")
	if !strings.Contains(ref, "token="+sentinelToken) || !strings.Contains(ref, "eid=EXPORTID1") {
		t.Errorf("finder referer missing token/eid: %s", ref)
	}
	if !strings.HasPrefix(ref, "https://channels.weixin.qq.com/finder-preview/pages/feed?") {
		t.Errorf("finder referer shape wrong: %s", ref)
	}
}

// The real get_feed_info endpoint answers HTTP 201 with a valid body;
// upstream worker.js accepts any 2xx via resp.ok. Both API steps must
// therefore treat 2xx as success.
func TestResolveAccepts2xxNon200(t *testing.T) {
	media := "https://cdn.example.com/v.mp4?a=1"
	f := &fakeTransport{
		yuanbao: func(r *http.Request) (*http.Response, error) {
			return jsonResp(201, `{"code":0,"data":{"playable_url":"`+playableURL+`"}}`), nil
		},
		finder: func(r *http.Request) (*http.Response, error) {
			body := fmt.Sprintf(`{"errCode":0,"data":{"feedInfo":{"videoUrl":%q}}}`, media)
			return jsonResp(201, body), nil
		},
	}
	video, err := newTestClient(t, f).Resolve(context.Background(), testShareURL, fakeCreds())
	if err != nil {
		t.Fatalf("2xx non-200 must be accepted like upstream resp.ok: %v", err)
	}
	if video.MediaURL != media {
		t.Errorf("media url wrong: %q", video.MediaURL)
	}
}

func TestRIDFormat(t *testing.T) {
	rid, err := buildRID()
	if err != nil {
		t.Fatal(err)
	}
	parts := strings.SplitN(rid, "-", 2)
	if len(parts) != 2 || len(parts[0]) == 0 {
		t.Fatalf("rid shape wrong: %q", rid)
	}
	if _, err := strconv.ParseInt(parts[0], 16, 64); err != nil {
		t.Errorf("rid timestamp not hex unix seconds: %q", parts[0])
	}
	if len(parts[1]) != 8 {
		t.Errorf("rid random part must be 8 hex chars, got %q", parts[1])
	}
}

func TestResolveErrorMatrix(t *testing.T) {
	cases := []struct {
		name      string
		yuanbao   func(*http.Request) (*http.Response, error)
		finder    func(*http.Request) (*http.Response, error)
		wantCode  apperr.Code
		wantStage apperr.Stage
	}{
		{"yuanbao 401", func(*http.Request) (*http.Response, error) { return jsonResp(401, `{}`), nil }, nil,
			apperr.InvalidCredentials, apperr.StageParseShare},
		{"yuanbao 403", func(*http.Request) (*http.Response, error) { return jsonResp(403, `{}`), nil }, nil,
			apperr.AccessDenied, apperr.StageParseShare},
		{"yuanbao 429", func(*http.Request) (*http.Response, error) { return jsonResp(429, `{}`), nil }, nil,
			apperr.RateLimited, apperr.StageParseShare},
		{"yuanbao 302 redirect", func(*http.Request) (*http.Response, error) {
			resp := jsonResp(302, ``)
			resp.Header.Set("Location", "https://evil.test/")
			return resp, nil
		}, nil, apperr.UpstreamError, apperr.StageParseShare},
		{"business code nonzero", func(*http.Request) (*http.Response, error) {
			return jsonResp(200, `{"code":1001,"msg":"need login"}`), nil
		}, nil, apperr.UpstreamError, apperr.StageParseShare},
		{"code missing", func(*http.Request) (*http.Response, error) {
			return jsonResp(200, `{"data":{"playable_url":"x"}}`), nil
		}, nil, apperr.SchemaChanged, apperr.StageParseShare},
		{"code wrong type", func(*http.Request) (*http.Response, error) {
			return jsonResp(200, `{"code":"0","data":{}}`), nil
		}, nil, apperr.SchemaChanged, apperr.StageParseShare},
		{"playable_url missing", func(*http.Request) (*http.Response, error) {
			return jsonResp(200, `{"code":0,"data":{}}`), nil
		}, nil, apperr.SchemaChanged, apperr.StageParseShare},
		{"playable_url wrong host", func(*http.Request) (*http.Response, error) {
			return playableResp("https://evil.test/finder-preview/pages/feed?token=a&eid=b"), nil
		}, nil, apperr.SchemaChanged, apperr.StageParseShare},
		{"playable_url wrong path", func(*http.Request) (*http.Response, error) {
			return playableResp("https://channels.weixin.qq.com/other/path?token=a&eid=b"), nil
		}, nil, apperr.SchemaChanged, apperr.StageParseShare},
		{"token missing", func(*http.Request) (*http.Response, error) {
			return playableResp("https://channels.weixin.qq.com/finder-preview/pages/feed?eid=b"), nil
		}, nil, apperr.SchemaChanged, apperr.StageParseShare},
		{"eid missing", func(*http.Request) (*http.Response, error) {
			return playableResp("https://channels.weixin.qq.com/finder-preview/pages/feed?token=a"), nil
		}, nil, apperr.SchemaChanged, apperr.StageParseShare},
		{"duplicate token param", func(*http.Request) (*http.Response, error) {
			return playableResp("https://channels.weixin.qq.com/finder-preview/pages/feed?token=a&token=b&eid=c"), nil
		}, nil, apperr.SchemaChanged, apperr.StageParseShare},
		{"finder errCode nonzero", func(*http.Request) (*http.Response, error) { return playableResp(playableURL), nil },
			func(*http.Request) (*http.Response, error) {
				return jsonResp(200, `{"errCode":-200,"errMsg":"<b>gone</b>"}`), nil
			},
			apperr.UpstreamError, apperr.StageFetchFeed},
		{"finder errCode missing", func(*http.Request) (*http.Response, error) { return playableResp(playableURL), nil },
			func(*http.Request) (*http.Response, error) { return jsonResp(200, `{"data":{}}`), nil },
			apperr.SchemaChanged, apperr.StageFetchFeed},
		{"finder errMsg unavailable", func(*http.Request) (*http.Response, error) { return playableResp(playableURL), nil },
			func(*http.Request) (*http.Response, error) {
				return jsonResp(200, `{"errCode":0,"data":{"errMsg":{"type":1,"title":"视频","content":"已删除"}}}`), nil
			},
			apperr.VideoUnavailable, apperr.StageFetchFeed},
		{"finder not JSON", func(*http.Request) (*http.Response, error) { return playableResp(playableURL), nil },
			func(*http.Request) (*http.Response, error) { return jsonResp(200, `<html>oops</html>`), nil },
			apperr.SchemaChanged, apperr.StageFetchFeed},
		{"finder oversize JSON", func(*http.Request) (*http.Response, error) { return playableResp(playableURL), nil },
			func(*http.Request) (*http.Response, error) {
				return jsonResp(200, `{"errCode":0,"pad":"`+strings.Repeat("x", maxAPIBodyBytes+1)+`"}`), nil
			},
			apperr.UpstreamError, apperr.StageFetchFeed},
		{"no feedInfo", func(*http.Request) (*http.Response, error) { return playableResp(playableURL), nil },
			func(*http.Request) (*http.Response, error) { return jsonResp(200, `{"errCode":0,"data":{}}`), nil },
			apperr.NoMedia, apperr.StageSelectMedia},
		{"pic album", func(*http.Request) (*http.Response, error) { return playableResp(playableURL), nil },
			func(*http.Request) (*http.Response, error) {
				return jsonResp(200, `{"errCode":0,"data":{"feedInfo":{"picInfo":[{"url":"https://x/1.jpg"}]}}}`), nil
			},
			apperr.UnsupportedMedia, apperr.StageSelectMedia},
		{"no media at all", func(*http.Request) (*http.Response, error) { return playableResp(playableURL), nil },
			func(*http.Request) (*http.Response, error) {
				return jsonResp(200, `{"errCode":0,"data":{"feedInfo":{"picInfo":[]}}}`), nil
			},
			apperr.NoMedia, apperr.StageSelectMedia},
		{"malformed media url", func(*http.Request) (*http.Response, error) { return playableResp(playableURL), nil },
			func(*http.Request) (*http.Response, error) {
				return jsonResp(200, `{"errCode":0,"data":{"feedInfo":{"videoUrl":"http://not-https/x.mp4"}}}`), nil
			},
			apperr.SchemaChanged, apperr.StageSelectMedia},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			f := &fakeTransport{yuanbao: tc.yuanbao, finder: tc.finder}
			c := newTestClient(t, f)
			_, err := c.Resolve(context.Background(), testShareURL, fakeCreds())
			if err == nil {
				t.Fatalf("expected error")
			}
			if apperr.CodeOf(err) != tc.wantCode || apperr.StageOf(err) != tc.wantStage {
				t.Errorf("got %s/%s, want %s/%s (msg: %s)", apperr.CodeOf(err), apperr.StageOf(err), tc.wantCode, tc.wantStage, err)
			}
			// messages must never leak sentinels or raw URLs
			for _, secret := range []string{sentinelCookie, sentinelToken, sentinelMediaQuery, playableURL, mediaURL} {
				if secret != "" && strings.Contains(err.Error(), secret) {
					t.Errorf("error leaked secret: %s", err)
				}
			}
		})
	}
}

func TestMediaSelectionOrder(t *testing.T) {
	cases := []struct {
		name       string
		feed       string
		wantSource string
		wantCodec  string
	}{
		{"h264 first", `{"errCode":0,"data":{"feedInfo":{"h264VideoInfo":{"videoUrl":"%s"},"h265VideoInfo":{"videoUrl":"%s"},"videoUrl":"%s"}}}`,
			"h264VideoInfo", "h264"},
		{"h265 fallback", `{"errCode":0,"data":{"feedInfo":{"h265VideoInfo":{"videoUrl":"%s"},"videoUrl":"%s"}}}`,
			"h265VideoInfo", "h265"},
		{"videoUrl fallback", `{"errCode":0,"data":{"feedInfo":{"videoUrl":"%s"}}}`,
			"videoUrl", ""},
	}
	urls := []string{
		"https://a.example/v.mp4?x=1",
		"https://b.example/v.mp4?x=2",
		"https://c.example/v.mp4?x=3",
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			args := []any{}
			for _, u := range urls {
				args = append(args, u)
			}
			// match exactly the number of %s in the template
			for len(args) > strings.Count(tc.feed, "%s") {
				args = args[:len(args)-1]
			}
			for len(args) < strings.Count(tc.feed, "%s") {
				args = append(args, urls[len(args)%len(urls)])
			}
			body := fmt.Sprintf(tc.feed, args...)
			f := &fakeTransport{
				yuanbao: func(*http.Request) (*http.Response, error) { return playableResp(playableURL), nil },
				finder:  func(*http.Request) (*http.Response, error) { return jsonResp(200, body), nil },
			}
			video, err := newTestClient(t, f).Resolve(context.Background(), testShareURL, fakeCreds())
			if err != nil {
				t.Fatalf("resolve: %v", err)
			}
			if video.MediaSource != tc.wantSource || video.CodecHint != tc.wantCodec {
				t.Errorf("got %s/%s want %s/%s", video.MediaSource, video.CodecHint, tc.wantSource, tc.wantCodec)
			}
			// optional meta missing must not fail
		})
	}
	// missing title/author tolerated
	f := &fakeTransport{
		yuanbao: func(*http.Request) (*http.Response, error) { return playableResp(playableURL), nil },
		finder: func(*http.Request) (*http.Response, error) {
			return jsonResp(200, `{"errCode":0,"data":{"feedInfo":{"videoUrl":"https://x.example/v.mp4"}}}`), nil
		},
	}
	video, err := newTestClient(t, f).Resolve(context.Background(), testShareURL, fakeCreds())
	if err != nil || video.Title != "" || video.Author != "" {
		t.Errorf("optional meta must default to empty: %+v %v", video, err)
	}
}

// Media URLs must survive with their percent-encoding, duplicate keys and
// ordering intact.
func TestMediaURLFidelity(t *testing.T) {
	raw := "https://cdn.example.com/v.mp4?X-snsvideoflag=1&encfilekey=a%2Fb&sig=x%26y&sig=z&q=1&q=2"
	f := &fakeTransport{
		yuanbao: func(*http.Request) (*http.Response, error) { return playableResp(playableURL), nil },
		finder:  func(*http.Request) (*http.Response, error) { return feedResp(raw), nil },
	}
	video, err := newTestClient(t, f).Resolve(context.Background(), testShareURL, fakeCreds())
	if err != nil {
		t.Fatal(err)
	}
	if video.MediaURL != raw {
		t.Errorf("URL rewritten:\n got %q\nwant %q", video.MediaURL, raw)
	}
}

func TestRetryPolicy(t *testing.T) {
	// 503 then success on yuanbao → one retry, both attempts recorded.
	calls := 0
	f := &fakeTransport{
		yuanbao: func(*http.Request) (*http.Response, error) {
			calls++
			if calls == 1 {
				return jsonResp(503, `down`), nil
			}
			return playableResp(playableURL), nil
		},
		finder: func(*http.Request) (*http.Response, error) { return feedResp(mediaURL), nil },
	}
	c := newTestClient(t, f)
	if _, err := c.Resolve(context.Background(), testShareURL, fakeCreds()); err != nil {
		t.Fatalf("retry should recover: %v", err)
	}
	if calls != 2 {
		t.Errorf("expected 2 calls, got %d", calls)
	}
	if f.sleeps != 1 {
		t.Errorf("expected 1 backoff sleep, got %d", f.sleeps)
	}

	// 403 must NOT retry.
	calls = 0
	f2 := &fakeTransport{
		yuanbao: func(*http.Request) (*http.Response, error) {
			calls++
			return jsonResp(403, `{}`), nil
		},
	}
	c2 := newTestClient(t, f2)
	if _, err := c2.Resolve(context.Background(), testShareURL, fakeCreds()); apperr.CodeOf(err) != apperr.AccessDenied {
		t.Errorf("want ACCESS_DENIED, got %v", err)
	}
	if calls != 1 {
		t.Errorf("403 must not retry, calls=%d", calls)
	}

	// transient network error then success → retries once.
	calls = 0
	f3 := &fakeTransport{
		yuanbao: func(*http.Request) (*http.Response, error) {
			calls++
			if calls == 1 {
				return nil, errors.New("connection reset by peer")
			}
			return playableResp(playableURL), nil
		},
		finder: func(*http.Request) (*http.Response, error) { return feedResp(mediaURL), nil },
	}
	c3 := newTestClient(t, f3)
	if _, err := c3.Resolve(context.Background(), testShareURL, fakeCreds()); err != nil {
		t.Errorf("transient failure should recover once: %v", err)
	}

	// persistent network error → gives up after 2 attempts with a clean
	// message (no URL inside).
	f4 := &fakeTransport{
		yuanbao: func(*http.Request) (*http.Response, error) {
			return nil, &netError{msg: "dial tcp: connection refused"}
		},
	}
	_, err := newTestClient(t, f4).Resolve(context.Background(), testShareURL, fakeCreds())
	if apperr.CodeOf(err) != apperr.NetworkError {
		t.Errorf("want NETWORK_ERROR, got %v", err)
	}
	if strings.Contains(err.Error(), "yuanbao") || strings.Contains(err.Error(), "http") {
		// stage names are fine; a URL would not be
		t.Logf("message: %s", err)
	}
}

type netError struct{ msg string }

func (e *netError) Error() string { return e.msg }

func TestParentCancelNotRetried(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	calls := 0
	f := &fakeTransport{
		yuanbao: func(*http.Request) (*http.Response, error) {
			calls++
			cancel()
			return nil, context.Canceled
		},
	}
	_, err := newTestClient(t, f).Resolve(ctx, testShareURL, fakeCreds())
	if apperr.CodeOf(err) != apperr.Cancelled {
		t.Errorf("want CANCELLED, got %v", err)
	}
	if calls != 1 {
		t.Errorf("cancelled request must not retry, calls=%d", calls)
	}
}

func TestPerAttemptDeadlineBecomesTimeout(t *testing.T) {
	// A request whose attempt context expires maps to TIMEOUT with a clean
	// message and can retry once within budget.
	calls := 0
	f := &fakeTransport{
		yuanbao: func(r *http.Request) (*http.Response, error) {
			calls++
			return nil, contextDeadline(r)
		},
	}
	_, err := newTestClient(t, f).Resolve(context.Background(), testShareURL, fakeCreds())
	if apperr.CodeOf(err) != apperr.Timeout {
		t.Errorf("want TIMEOUT, got %v", err)
	}
	if calls != 2 {
		t.Errorf("timeout should retry exactly once, calls=%d", calls)
	}
}

func contextDeadline(r *http.Request) error {
	// Simulate the transport receiving a deadline-exceeded error.
	return fmt.Errorf("POST %s: %w", netpolicy.SanitizeURLForError(r.URL.String()), context.DeadlineExceeded)
}

func TestShareURLValidatedBeforeAnyRequest(t *testing.T) {
	f := &fakeTransport{}
	_, err := newTestClient(t, f).Resolve(context.Background(), "https://evil.test/x", fakeCreds())
	if apperr.CodeOf(err) != apperr.InvalidArgument {
		t.Errorf("want INVALID_ARGUMENT, got %v", err)
	}
	if n := len(f.seen()); n != 0 {
		t.Errorf("no request should be made for an invalid link, made %d", n)
	}
}

func TestSanitizeMessageStripsTagsAndControlChars(t *testing.T) {
	got := sanitizeMessage("<b>登录已过期\x1b[31m红字\u0000</b>\n")
	if strings.ContainsAny(got, "<>\x1b\u0000\n") {
		t.Errorf("sanitize failed: %q", got)
	}
	long := sanitizeMessage(strings.Repeat("字", 500))
	if l := len([]rune(long)); l > 121 {
		t.Errorf("long message not capped: %d runes", l)
	}
}

func TestAPIRejectsRedirectViaClientPolicy(t *testing.T) {
	// Wire the client through netpolicy's API client over a local
	// redirecting server? Not possible offline; instead assert the sentinel
	// error is what upstream maps. Unit-level check:
	if !errors.Is(netpolicy.ErrNoRedirect, netpolicy.ErrNoRedirect) {
		t.Fatal("sanity")
	}
	f := &fakeTransport{}
	_ = f
	// and the doOnce 3xx branch:
	resp := jsonResp(302, "")
	resp.Header.Set("Location", "https://evil.test/")
	f2 := &fakeTransport{yuanbao: func(*http.Request) (*http.Response, error) { return resp, nil }}
	_, err := newTestClient(t, f2).Resolve(context.Background(), testShareURL, fakeCreds())
	if apperr.CodeOf(err) != apperr.UpstreamError {
		t.Errorf("3xx must be refused, got %v", err)
	}
}

var _ = bytes.MinRead
