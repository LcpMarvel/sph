package netpolicy

import (
	"context"
	"crypto/tls"
	"crypto/x509"
	"errors"
	"fmt"
	"net"
	"net/http"
	"net/url"
	"syscall"
	"time"

	"github.com/LcpMarvel/sph-downloader/internal/apperr"
)

// Timeouts
const (
	ConnectTimeout     = 10 * time.Second
	TLSHandshake       = 10 * time.Second
	ResponseHeaderWait = 30 * time.Second
	MaxMediaRedirects  = 5
)

var errBlockedAddress = errors.New("目标地址不是公网地址，已按网络策略拒绝连接")

// isPublicIP reports whether ip is a public unicast address. Loopback,
// private, link-local, multicast, broadcast, "this network" and unspecified
// addresses are rejected.
func isPublicIP(ip net.IP) bool {
	if ip == nil || ip.IsUnspecified() || ip.IsLoopback() || ip.IsMulticast() ||
		ip.IsPrivate() || ip.IsLinkLocalUnicast() || ip.IsLinkLocalMulticast() ||
		ip.Equal(net.IPv4bcast) || (ip.To4() != nil && ip.To4()[0] == 0) {
		return false
	}
	return true
}

// SafeDial returns a DialContext that resolves the hostname, requires every
// resolved address to be public, then dials the address directly. TLS host
// verification stays tied to the original URL hostname because the dial
// happens below the TLS layer.
func SafeDial(parent context.Context) func(ctx context.Context, network, addr string) (net.Conn, error) {
	dialer := &net.Dialer{Timeout: ConnectTimeout, KeepAlive: 30 * time.Second}
	resolver := net.DefaultResolver
	return func(ctx context.Context, network, addr string) (net.Conn, error) {
		host, port, err := net.SplitHostPort(addr)
		if err != nil {
			return nil, fmt.Errorf("地址格式错误: %w", err)
		}
		if ip := net.ParseIP(host); ip != nil {
			if !isPublicIP(ip) {
				return nil, errBlockedAddress
			}
			return dialer.DialContext(ctx, network, net.JoinHostPort(host, port))
		}
		ips, err := resolver.LookupIPAddr(ctx, host)
		if err != nil {
			return nil, err
		}
		if len(ips) == 0 {
			return nil, fmt.Errorf("主机 %s 未解析到任何地址", host)
		}
		for _, ia := range ips {
			if !isPublicIP(ia.IP) {
				return nil, errBlockedAddress
			}
		}
		return dialer.DialContext(ctx, network, net.JoinHostPort(ips[0].IP.String(), port))
	}
}

// NewTransport builds the shared http.Transport: no proxy inheritance, no
// transparent compression (byte accounting must match what is stored),
// public-address dialing, TLS verification left on.
func NewTransport() *http.Transport {
	return &http.Transport{
		Proxy:                 nil,
		DisableCompression:    true,
		DialContext:           SafeDial(context.Background()),
		ForceAttemptHTTP2:     false,
		MaxIdleConns:          4,
		IdleConnTimeout:       30 * time.Second,
		TLSHandshakeTimeout:   TLSHandshake,
		ResponseHeaderTimeout: ResponseHeaderWait,
	}
}

// ErrNoRedirect makes http.Client stop at the first redirect; the caller then
// sees the 3xx response itself and reports a controlled error.
var ErrNoRedirect = errors.New("接口不应重定向")

// NewAPIClient returns a client for the fixed yuanbao/finder endpoints. It
// never follows redirects.
func NewAPIClient() *http.Client {
	return &http.Client{
		Transport: NewTransport(),
		CheckRedirect: func(req *http.Request, via []*http.Request) error {
			return ErrNoRedirect
		},
	}
}

// NewMediaClient returns a client for media servers. It follows at most 5
// redirects and re-validates every hop with ValidateMediaURL, so session
// headers can never leak to an unexpected destination. No cookie
// jar is installed, so no credentials travel on redirects either.
func NewMediaClient() *http.Client {
	return &http.Client{
		Transport: NewTransport(),
		CheckRedirect: func(req *http.Request, via []*http.Request) error {
			if len(via) >= MaxMediaRedirects {
				return fmt.Errorf("媒体重定向超过 %d 跳", MaxMediaRedirects)
			}
			if err := ValidateMediaURL(req.URL.String()); err != nil {
				return fmt.Errorf("媒体重定向目标被拒绝")
			}
			return nil
		},
	}
}

// SanitizeURLForError strips query and userinfo from a URL so that a
// *url.Error can be reported without leaking signed query strings.
func SanitizeURLForError(rawURL string) string {
	u, err := url.Parse(rawURL)
	if err != nil {
		return "<无法解析的 URL>"
	}
	u.User = nil
	u.RawQuery = ""
	u.Fragment = ""
	return u.String()
}

// WrapNetError classifies a raw transport error into the apperr model without
// embedding the original URL or error text.
func WrapNetError(err error, stage apperr.Stage, prefix string) *apperr.Error {
	if err == nil {
		return nil
	}
	if errors.Is(err, context.Canceled) {
		return apperr.Wrap(err, apperr.Cancelled, stage, prefix+"已取消")
	}
	if errors.Is(err, context.DeadlineExceeded) {
		return apperr.Wrap(err, apperr.Timeout, stage, prefix+"超时")
	}
	if errors.Is(err, errBlockedAddress) {
		return apperr.Wrap(err, apperr.NetworkError, stage, prefix+"目标地址被本工具网络策略拒绝（不允许回环/私网地址）")
	}
	var dnsErr *net.DNSError
	if errors.As(err, &dnsErr) {
		return apperr.Wrap(err, apperr.NetworkError, stage, prefix+"域名解析失败")
	}
	if errors.Is(err, syscall.ECONNREFUSED) || errors.Is(err, syscall.ECONNRESET) ||
		errors.Is(err, syscall.ECONNABORTED) || errors.Is(err, syscall.EHOSTUNREACH) ||
		errors.Is(err, syscall.ENETUNREACH) || errors.Is(err, syscall.ETIMEDOUT) {
		return apperr.Wrap(err, apperr.NetworkError, stage, prefix+"网络连接失败")
	}
	var hostErr x509.HostnameError
	var unknownAuthErr x509.UnknownAuthorityError
	var recordErr tls.RecordHeaderError
	if errors.As(err, &hostErr) || errors.As(err, &unknownAuthErr) || errors.As(err, &recordErr) {
		return apperr.Wrap(err, apperr.NetworkError, stage, prefix+"TLS 校验失败")
	}
	return apperr.Wrap(err, apperr.NetworkError, stage, prefix+"网络请求失败")
}
