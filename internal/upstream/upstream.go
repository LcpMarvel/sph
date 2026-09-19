// Package upstream implements the two-step parse chain verified against the
// pinned upstream commit:
//
//  1. POST https://yuanbao.tencent.com/api/weixin/get_parse_result
//     → data.playable_url
//  2. POST https://channels.weixin.qq.com/finder-preview/api/feed/get_feed_info
//     → data.feedInfo.* video URL selection
//
// Resolve is the single validator reused by inspect, download and login.
package upstream

import (
	"bytes"
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"
	"time"
	"unicode/utf8"

	"sph/internal/apperr"
	"sph/internal/auth"
	"sph/internal/media"
	"sph/internal/netpolicy"
)

// HTTPClient is the injectable request interface (tests use fakes).
type HTTPClient interface {
	Do(req *http.Request) (*http.Response, error)
}

const (
	DefaultYuanbaoBase = "https://yuanbao.tencent.com"
	DefaultFinderBase  = "https://channels.weixin.qq.com"

	yuanbaoParsePath = "/api/weixin/get_parse_result"
	finderFeedPath   = "/finder-preview/api/feed/get_feed_info"
	finderPagePath   = "/finder-preview/pages/feed"

	// DefaultUserAgent is a fixed tool identifier. The user's own
	// captured User-Agent overrides it when imported.
	DefaultUserAgent = "sph-local/1.0"

	apiRequestTimeout = 30 * time.Second
	maxAPIBodyBytes   = 4 << 20 // 4 MiB
	retryDelay        = time.Second
)

// Client performs the parse chain. Base URLs are overridable for tests.
type Client struct {
	HTTP        HTTPClient
	YuanbaoBase string
	FinderBase  string
	Sleep       func(ctx context.Context, d time.Duration) error
}

// NewClient returns a Client wired to production endpoints.
func NewClient(httpClient HTTPClient) *Client {
	return &Client{
		HTTP:        httpClient,
		YuanbaoBase: DefaultYuanbaoBase,
		FinderBase:  DefaultFinderBase,
		Sleep:       sleepCtx,
	}
}

func sleepCtx(ctx context.Context, d time.Duration) error {
	t := time.NewTimer(d)
	defer t.Stop()
	select {
	case <-ctx.Done():
		return ctx.Err()
	case <-t.C:
		return nil
	}
}

// ResolvedVideo mirrors media.ResolvedVideo here for readability.
type ResolvedVideo = media.ResolvedVideo

// Resolve runs the whole parse chain for one share URL with the given
// credentials. It is the only validator: login reuses it verbatim.
func (c *Client) Resolve(ctx context.Context, shareURL string, creds auth.Credentials) (ResolvedVideo, error) {
	normalized, err := netpolicy.NormalizeShareURL(shareURL)
	if err != nil {
		return ResolvedVideo{}, err
	}
	playable, err := c.parseShare(ctx, normalized, creds)
	if err != nil {
		return ResolvedVideo{}, err
	}
	token, eid, err := extractPreviewParams(playable)
	if err != nil {
		return ResolvedVideo{}, err
	}
	feed, err := c.fetchFeed(ctx, token, eid, creds)
	if err != nil {
		return ResolvedVideo{}, err
	}
	video, err := selectMedia(feed)
	if err != nil {
		return ResolvedVideo{}, err
	}
	video.ShareURL = normalized
	video.LocalID = media.LocalID(normalized)
	return video, nil
}

// --- step 1: yuanbao parse ---------------------------------------------

type yuanbaoResponse struct {
	Code *int64          `json:"code"` // must be present and numeric
	Msg  string          `json:"msg"`
	Data json.RawMessage `json:"data"`
}

type yuanbaoData struct {
	PlayableURL string `json:"playable_url"`
}

func (c *Client) parseShare(ctx context.Context, normalizedURL string, creds auth.Credentials) (string, error) {
	body, err := json.Marshal(map[string]any{
		"type":  "video_channel_url",
		"url":   normalizedURL,
		"scene": 1,
	})
	if err != nil {
		return "", apperr.Wrap(err, apperr.InternalError, apperr.StageParseShare, "构造元宝请求失败")
	}
	resp, err := c.doWithRetry(ctx, apperr.StageParseShare, "元宝解析", func() (*http.Request, error) {
		req, err := http.NewRequest(http.MethodPost, c.YuanbaoBase+yuanbaoParsePath, bytes.NewReader(body))
		if err != nil {
			return nil, err
		}
		req.Header.Set("Accept", "application/json, text/plain, */*")
		req.Header.Set("Content-Type", "application/json")
		req.Header.Set("Origin", c.YuanbaoBase)
		req.Header.Set("Referer", c.YuanbaoBase+"/")
		req.Header.Set("User-Agent", DefaultUserAgent)
		req.Header.Set("Cookie", creds.Cookie)
		for k, v := range creds.YuanbaoHeaders {
			req.Header.Set(k, v)
		}
		return req, nil
	})
	if err != nil {
		return "", err
	}
	defer drain(resp)
	if err := mapAPIStatus(resp.StatusCode, apperr.StageParseShare, "元宝解析接口"); err != nil {
		return "", err
	}
	raw, err := readBounded(resp.Body, maxAPIBodyBytes, apperr.StageParseShare)
	if err != nil {
		return "", err
	}
	var yr yuanbaoResponse
	if err := json.Unmarshal(raw, &yr); err != nil {
		return "", apperr.New(apperr.SchemaChanged, apperr.StageParseShare, "元宝响应不是有效 JSON")
	}
	if yr.Code == nil {
		return "", apperr.New(apperr.SchemaChanged, apperr.StageParseShare, "元宝响应缺少数字 code 字段")
	}
	if *yr.Code != 0 {
		return "", apperr.Newf(apperr.UpstreamError, apperr.StageParseShare, "元宝业务错误 code %d: %s", *yr.Code, sanitizeMessage(yr.Msg))
	}
	var yd yuanbaoData
	if err := json.Unmarshal(yr.Data, &yd); err != nil || yd.PlayableURL == "" {
		return "", apperr.New(apperr.SchemaChanged, apperr.StageParseShare, "元宝响应缺少 data.playable_url")
	}
	return yd.PlayableURL, nil
}

// extractPreviewParams pulls generalToken and exportId out of the
// playable_url per: exactly one "token" and one "eid", both
// non-empty after a single query decode.
func extractPreviewParams(playableURL string) (string, string, error) {
	u, err := netpolicy.ValidatePreviewURL(playableURL)
	if err != nil {
		return "", "", err
	}
	counts := netpolicy.CountQueryKeys(u.RawQuery)
	if counts["token"] > 1 || counts["eid"] > 1 {
		return "", "", apperr.New(apperr.SchemaChanged, apperr.StageParseShare, "playable_url 的 token/eid 参数重复")
	}
	q := u.Query()
	token := q.Get("token")
	eid := q.Get("eid")
	if token == "" || eid == "" {
		return "", "", apperr.New(apperr.SchemaChanged, apperr.StageParseShare, "playable_url 缺少 token 或 eid")
	}
	return token, eid, nil
}

// --- step 2: finder preview feed ----------------------------------------

func (c *Client) fetchFeed(ctx context.Context, token, eid string, creds auth.Credentials) (*finderResponse, error) {
	rid, err := buildRID()
	if err != nil {
		return nil, apperr.Wrap(err, apperr.InternalError, apperr.StageFetchFeed, "生成请求标识失败")
	}
	query := url.Values{}
	query.Set("_rid", rid)
	query.Set("_pageUrl", c.FinderBase+finderPagePath)
	body, err := json.Marshal(finderRequestBody{
		BaseReq:  finderBaseReq{GeneralToken: token},
		ExportID: eid,
	})
	if err != nil {
		return nil, apperr.Wrap(err, apperr.InternalError, apperr.StageFetchFeed, "构造预览请求失败")
	}
	endpoint := c.FinderBase + finderFeedPath + "?" + query.Encode()
	referer := buildFinderReferer(token, eid)
	// Only the non-sensitive User-Agent may be reused from the yuanbao
	// session; every other session header and the cookie stay behind
	//.
	userAgent := DefaultUserAgent
	if ua, ok := creds.YuanbaoHeaders["user-agent"]; ok && ua != "" {
		userAgent = ua
	}
	resp, err := c.doWithRetry(ctx, apperr.StageFetchFeed, "视频号预览", func() (*http.Request, error) {
		req, err := http.NewRequest(http.MethodPost, endpoint, bytes.NewReader(body))
		if err != nil {
			return nil, err
		}
		req.Header.Set("Accept", "application/json, text/plain, */*")
		req.Header.Set("Content-Type", "application/json")
		req.Header.Set("Origin", c.FinderBase)
		req.Header.Set("Referer", referer)
		req.Header.Set("User-Agent", userAgent)
		return req, nil
	})
	if err != nil {
		return nil, err
	}
	defer drain(resp)
	if err := mapAPIStatus(resp.StatusCode, apperr.StageFetchFeed, "视频号预览接口"); err != nil {
		return nil, err
	}
	raw, err := readBounded(resp.Body, maxAPIBodyBytes, apperr.StageFetchFeed)
	if err != nil {
		return nil, err
	}
	fr, err := parseFinderResponse(raw)
	if err != nil {
		return nil, err
	}
	return fr, nil
}

type finderBaseReq struct {
	GeneralToken string `json:"generalToken"`
}

type finderRequestBody struct {
	BaseReq  finderBaseReq `json:"baseReq"`
	ExportID string        `json:"exportId"`
}

func buildRID() (string, error) {
	buf := make([]byte, 4)
	if _, err := rand.Read(buf); err != nil {
		return "", err
	}
	return fmt.Sprintf("%x-%s", time.Now().Unix(), hex.EncodeToString(buf)), nil
}

func buildFinderReferer(token, eid string) string {
	q := url.Values{}
	q.Set("entry_card_type", "48")
	q.Set("comment_scene", "39")
	q.Set("appid", "0")
	q.Set("token", token)
	q.Set("entry_scene", "0")
	q.Set("eid", eid)
	u := url.URL{
		Scheme:   "https",
		Host:     "channels.weixin.qq.com",
		Path:     finderPagePath,
		RawQuery: q.Encode(),
	}
	return u.String()
}

// finderResponse models the parts of get_feed_info this tool needs.
type finderResponse struct {
	ErrCode *int64             `json:"errCode"`
	ErrMsg  string             `json:"errMsg"`
	Data    finderResponseData `json:"data"`
}

type finderResponseData struct {
	ErrMsg     *finderErrMsg `json:"errMsg"`
	FeedInfo   *feedInfo     `json:"feedInfo"`
	AuthorInfo *authorInfo   `json:"authorInfo"`
}

type finderErrMsg struct {
	Type    int    `json:"type"`
	Title   string `json:"title"`
	Content string `json:"content"`
}

type feedInfo struct {
	H264VideoInfo *videoInfo `json:"h264VideoInfo"`
	H265VideoInfo *videoInfo `json:"h265VideoInfo"`
	VideoURL      string     `json:"videoUrl"`
	Description   string     `json:"description"`
	PicInfo       []picInfo  `json:"picInfo"`
}

type videoInfo struct {
	VideoURL string `json:"videoUrl"`
}

type picInfo struct {
	URL string `json:"url"`
}

type authorInfo struct {
	Nickname string `json:"nickname"`
}

func parseFinderResponse(raw []byte) (*finderResponse, error) {
	var fr finderResponse
	if err := json.Unmarshal(raw, &fr); err != nil {
		return nil, apperr.New(apperr.SchemaChanged, apperr.StageFetchFeed, "预览接口响应不是有效 JSON")
	}
	if fr.ErrCode == nil {
		return nil, apperr.New(apperr.SchemaChanged, apperr.StageFetchFeed, "预览接口响应缺少数字 errCode")
	}
	if *fr.ErrCode != 0 {
		// Unknown numeric codes stay numeric for diagnosis; they are NOT
		// guessed into "cookie expired".
		return nil, apperr.Newf(apperr.UpstreamError, apperr.StageFetchFeed,
			"预览接口业务错误 errCode %d: %s", *fr.ErrCode, sanitizeMessage(fr.ErrMsg))
	}
	if em := fr.Data.ErrMsg; em != nil && (em.Type != 0 || strings.TrimSpace(em.Title) != "" || strings.TrimSpace(em.Content) != "") {
		msg := strings.Trim(strings.Join([]string{
			sanitizeMessage(em.Title), sanitizeMessage(em.Content)}, ": "), ": ")
		if msg == "" {
			msg = fmt.Sprintf("视频不可用 (type %d)", em.Type)
		}
		return nil, apperr.New(apperr.VideoUnavailable, apperr.StageFetchFeed, "视频不可用: "+msg)
	}
	return &fr, nil
}

// selectMedia picks the first non-empty address per the upstream page order
// : h264 → h265 → videoUrl. The chosen URL string is used verbatim.
func selectMedia(fr *finderResponse) (ResolvedVideo, error) {
	empty := ResolvedVideo{}
	fi := fr.Data.FeedInfo
	if fi == nil {
		return empty, apperr.New(apperr.NoMedia, apperr.StageSelectMedia, "响应中没有可用的视频信息")
	}
	pick := func(url, source, codec string) (ResolvedVideo, bool) {
		if url == "" {
			return empty, false
		}
		if err := netpolicy.ValidateMediaURL(url); err != nil {
			// malformed address is a structural problem, not a fallback trigger
			return empty, false
		}
		return ResolvedVideo{MediaURL: url, MediaSource: source, CodecHint: codec}, true
	}
	if fi.H264VideoInfo != nil {
		if v, ok := pick(fi.H264VideoInfo.VideoURL, "h264VideoInfo", "h264"); ok {
			return withMeta(v, fi, fr), nil
		}
		if fi.H264VideoInfo.VideoURL != "" {
			return empty, apperr.New(apperr.SchemaChanged, apperr.StageSelectMedia, "h264 视频地址不合法")
		}
	}
	if fi.H265VideoInfo != nil {
		if v, ok := pick(fi.H265VideoInfo.VideoURL, "h265VideoInfo", "h265"); ok {
			return withMeta(v, fi, fr), nil
		}
		if fi.H265VideoInfo.VideoURL != "" {
			return empty, apperr.New(apperr.SchemaChanged, apperr.StageSelectMedia, "h265 视频地址不合法")
		}
	}
	if fi.VideoURL != "" {
		if v, ok := pick(fi.VideoURL, "videoUrl", ""); ok {
			return withMeta(v, fi, fr), nil
		}
		return empty, apperr.New(apperr.SchemaChanged, apperr.StageSelectMedia, "视频地址不合法")
	}
	if len(fi.PicInfo) > 0 {
		return empty, apperr.New(apperr.UnsupportedMedia, apperr.StageSelectMedia, "该内容是图集，本版本不支持")
	}
	return empty, apperr.New(apperr.NoMedia, apperr.StageSelectMedia, "没有可用的视频地址")
}

func withMeta(v ResolvedVideo, fi *feedInfo, fr *finderResponse) ResolvedVideo {
	v.Title = fi.Description
	if fr.Data.AuthorInfo != nil {
		v.Author = fr.Data.AuthorInfo.Nickname
	}
	return v
}

// --- shared HTTP plumbing ----------------------------------------------

// transientHTTPStatus reports whether a status is a transient upstream issue
// eligible for the single retry.
func transientHTTPStatus(code int) bool {
	return code == http.StatusBadGateway || code == http.StatusServiceUnavailable || code == http.StatusGatewayTimeout
}

func (c *Client) doWithRetry(ctx context.Context, stage apperr.Stage, what string, build func() (*http.Request, error)) (*http.Response, error) {
	resp, err := c.doOnce(ctx, stage, what, build)
	if err != nil && !shouldRetryRequest(ctx, err, nil) {
		return nil, err
	}
	if err == nil && !shouldRetryRequest(ctx, nil, resp) {
		return resp, nil
	}
	if c.Sleep == nil {
		c.Sleep = sleepCtx
	}
	if serr := c.Sleep(ctx, retryDelay); serr != nil {
		if err != nil {
			return nil, err
		}
		return resp, nil
	}
	if resp != nil {
		drain(resp)
	}
	resp2, err2 := c.doOnce(ctx, stage, what, build)
	if err2 != nil {
		return nil, err2
	}
	return resp2, nil
}

func (c *Client) doOnce(ctx context.Context, stage apperr.Stage, what string, build func() (*http.Request, error)) (*http.Response, error) {
	if c.HTTP == nil {
		return nil, apperr.New(apperr.InternalError, stage, "未配置 HTTP 客户端")
	}
	req, err := build()
	if err != nil {
		return nil, apperr.Wrap(err, apperr.InternalError, stage, "构造请求失败")
	}
	actx, cancel := context.WithTimeout(ctx, apiRequestTimeout)
	defer cancel()
	resp, err := c.HTTP.Do(req.WithContext(actx))
	if err != nil {
		// url.Error embeds the full request URL; never render it.
		if errors.Is(err, netpolicy.ErrNoRedirect) {
			return nil, apperr.New(apperr.UpstreamError, stage, what+"接口返回重定向，已拒绝跟随")
		}
		if ctxErr := ctx.Err(); ctxErr != nil {
			return nil, classifyParentCancel(ctxErr, stage, what)
		}
		return nil, netpolicy.WrapNetError(err, stage, what+"请求失败: ")
	}
	if resp.StatusCode >= 300 && resp.StatusCode < 400 {
		drain(resp)
		return nil, apperr.Newf(apperr.UpstreamError, stage, "%s接口返回 HTTP %d 重定向，已拒绝跟随", what, resp.StatusCode)
	}
	return resp, nil
}

func classifyParentCancel(ctxErr error, stage apperr.Stage, what string) *apperr.Error {
	if errors.Is(ctxErr, context.Canceled) {
		return apperr.Wrap(ctxErr, apperr.Cancelled, stage, what+"已取消")
	}
	return apperr.Wrap(ctxErr, apperr.Timeout, stage, what+"超时")
}

// shouldRetryRequest implements the "at most one retry, shared deadline"
// policy: transient network failures and 502/503/504 only; a canceled parent
// context is terminal.
func shouldRetryRequest(ctx context.Context, err error, resp *http.Response) bool {
	if ctx.Err() != nil {
		return false
	}
	if resp != nil {
		return transientHTTPStatus(resp.StatusCode)
	}
	if err == nil {
		return false
	}
	var ae *apperr.Error
	if errors.As(err, &ae) {
		if ae.Code == apperr.Timeout || ae.Code == apperr.Cancelled {
			// attempt-level timeouts are transient only while the parent
			// deadline has not fired
			return ctx.Err() == nil
		}
		return ae.Code == apperr.NetworkError
	}
	return false
}

// mapAPIStatus classifies an API HTTP status. Any 2xx is a success — the
// real get_feed_info endpoint has been observed answering HTTP 201 with a
// valid JSON body, and upstream worker.js accepts it via fetch's resp.ok
// semantics. Strict 200-only is reserved for media responses.
func mapAPIStatus(status int, stage apperr.Stage, what string) error {
	switch {
	case status >= 200 && status < 300:
		return nil
	case status == http.StatusUnauthorized:
		return apperr.New(apperr.InvalidCredentials, stage, "登录凭证不可用，请执行 sph login 重新登录。")
	case status == http.StatusForbidden:
		return apperr.New(apperr.AccessDenied, stage, what+"拒绝访问 (HTTP 403)")
	case status == http.StatusTooManyRequests:
		return apperr.New(apperr.RateLimited, stage, what+"限流 (HTTP 429)，请稍后重试")
	default:
		return apperr.Newf(apperr.UpstreamError, stage, "%s返回 HTTP %d", what, status)
	}
}

func readBounded(r io.Reader, limit int64, stage apperr.Stage) ([]byte, error) {
	limited := io.LimitReader(r, limit+1)
	raw, err := io.ReadAll(limited)
	if err != nil {
		return nil, netpolicy.WrapNetError(err, stage, "读取响应失败: ")
	}
	if int64(len(raw)) > limit {
		return nil, apperr.Newf(apperr.UpstreamError, stage, "响应超过 %d 字节上限", limit)
	}
	return raw, nil
}

func drain(resp *http.Response) {
	if resp == nil || resp.Body == nil {
		return
	}
	io.Copy(io.Discard, io.LimitReader(resp.Body, 4096))
	resp.Body.Close()
}

// sanitizeMessage makes an upstream-provided message safe to print: control
// characters removed, HTML tags stripped, length capped.
func sanitizeMessage(msg string) string {
	var b strings.Builder
	insideTag := false
	for _, r := range msg {
		switch {
		case r == '<':
			insideTag = true
		case r == '>':
			insideTag = false
		case insideTag:
		case r < 0x20 || r == 0x7f:
		default:
			b.WriteRune(r)
		}
	}
	out := strings.TrimSpace(b.String())
	if utf8.RuneCountInString(out) > 120 {
		runes := []rune(out)
		out = string(runes[:120]) + "…"
	}
	return out
}
