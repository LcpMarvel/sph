// Package download streams a single media file to disk under the byte cap,
// verifies it, and commits it atomically with default no-clobber semantics
// .
package download

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"strings"

	"github.com/LcpMarvel/sph-downloader/internal/apperr"
	"github.com/LcpMarvel/sph-downloader/internal/media"
	"github.com/LcpMarvel/sph-downloader/internal/netpolicy"
	"github.com/LcpMarvel/sph-downloader/internal/verify"
)

// HTTPClient is the injectable request interface.
type HTTPClient interface {
	Do(req *http.Request) (*http.Response, error)
}

// DefaultMaxBytes is 2 GiB.
const DefaultMaxBytes = int64(2147483648)

const (
	copyBufferBytes = 256 << 10 // 256 KiB
	sniffBytes      = 64
)

// Options configures one download.
type Options struct {
	// OutputPath is the absolute target path; empty means auto-name inside
	// WorkDir as <safe-title>_<local_id>.mp4.
	OutputPath string
	WorkDir    string
	Overwrite  bool
	MaxBytes   int64
	HTTP       HTTPClient
	// Verify runs the file checks; defaults to verify.Verify. Returns
	// "ffprobe" or "container".
	Verify func(ctx context.Context, path string) (string, error)
	// Progress is optional; the caller throttles its frequency.
	Progress func(sent, total int64)
	// Warn reports non-fatal cleanup problems (residual temp files).
	Warn func(msg string)
}

// Result is the safe DTO for download success.
type Result struct {
	LocalID      string `json:"local_id"`
	Path         string `json:"path"`
	Bytes        int64  `json:"bytes"`
	SHA256       string `json:"sha256"`
	Verification string `json:"verification"`
}

// Downloader runs downloads for one resolved video.
type Downloader struct {
	opts    Options
	video   media.ResolvedVideo
	tmpPath string
}

// New validates options and prepares the target path before any network
// activity.
func New(video media.ResolvedVideo, opts Options) (*Downloader, error) {
	if opts.MaxBytes == 0 {
		opts.MaxBytes = DefaultMaxBytes
	}
	if opts.MaxBytes < 0 {
		return nil, apperr.New(apperr.InvalidArgument, apperr.StageArguments, "--max-bytes 必须是正整数")
	}
	if opts.HTTP == nil {
		opts.HTTP = netpolicy.NewMediaClient()
	}
	if opts.Verify == nil {
		opts.Verify = verify.Verify
	}
	if opts.WorkDir == "" {
		if opts.OutputPath == "" {
			return nil, apperr.New(apperr.InvalidArgument, apperr.StageArguments, "未指定输出目录")
		}
		opts.WorkDir = filepath.Dir(opts.OutputPath)
	}
	target := opts.OutputPath
	if target == "" {
		target = filepath.Join(opts.WorkDir, media.SafeTitle(video.Title)+"_"+video.LocalID+".mp4")
	} else {
		if !strings.HasSuffix(strings.ToLower(target), ".mp4") {
			return nil, apperr.New(apperr.InvalidArgument, apperr.StageArguments, "输出文件必须以 .mp4 结尾")
		}
		parent := filepath.Dir(target)
		info, err := os.Stat(parent)
		if err != nil || !info.IsDir() {
			return nil, apperr.New(apperr.InvalidArgument, apperr.StageArguments, "输出路径的父目录不存在")
		}
	}
	abs, err := filepath.Abs(target)
	if err != nil {
		return nil, apperr.Wrap(err, apperr.IOError, apperr.StageArguments, "无法确定输出路径的绝对形式")
	}
	if info, err := os.Lstat(abs); err == nil {
		if info.IsDir() {
			return nil, apperr.New(apperr.InvalidArgument, apperr.StageArguments, "输出路径是目录")
		}
		if info.Mode()&os.ModeSymlink != 0 {
			return nil, apperr.New(apperr.InvalidArgument, apperr.StageArguments, "输出路径是符号链接，已拒绝")
		}
		if !opts.Overwrite {
			return nil, apperr.Newf(apperr.FileExists, apperr.StageCommit, "目标文件已存在：%s（使用 --overwrite 覆盖）", abs)
		}
	} else if !os.IsNotExist(err) {
		return nil, apperr.Wrap(err, apperr.IOError, apperr.StageArguments, "无法检查输出路径")
	}
	opts.OutputPath = abs
	return &Downloader{opts: opts, video: video}, nil
}

// Target is the absolute output path chosen in New.
func (d *Downloader) Target() string { return d.opts.OutputPath }

// Run performs the download: fetch → temp file → verify → atomic commit.
// Every failure path removes the temp file it created.
func (d *Downloader) Run(ctx context.Context) (Result, error) {
	result, err := d.run(ctx)
	if err != nil && d.tmpPath != "" {
		if rmErr := os.Remove(d.tmpPath); rmErr != nil && !os.IsNotExist(rmErr) {
			// never hide the original error, but surface the residual path
			// so the user can clean it up
			if d.opts.Warn != nil {
				d.opts.Warn("清理临时文件失败，请手动删除 " + d.tmpPath)
			}
		} else {
			d.tmpPath = ""
		}
	}
	return result, err
}

func (d *Downloader) run(ctx context.Context) (Result, error) {
	// up to two attempts; the second always starts from scratch
	var lastErr error
	for attempt := 0; attempt < 2; attempt++ {
		if attempt > 0 {
			if ctx.Err() != nil {
				break
			}
			if d.tmpPath != "" {
				os.Remove(d.tmpPath)
				d.tmpPath = ""
			}
		}
		bytes, sum, err := d.fetchToTemp(ctx)
		if err == nil {
			return d.commit(ctx, bytes, sum)
		}
		lastErr = err
		if !isRetryableTransfer(err) || ctx.Err() != nil {
			return Result{}, err
		}
	}
	return Result{}, lastErr
}

// fetchToTemp streams the media response into a fresh 0600 temp file in the
// target directory, incrementally hashing, and returns (bytes, sha256hex).
func (d *Downloader) fetchToTemp(ctx context.Context) (int64, string, error) {
	req, err := http.NewRequest(http.MethodGet, d.video.MediaURL, nil)
	if err != nil {
		return 0, "", apperr.New(apperr.SchemaChanged, apperr.StageDownload, "媒体地址无法用于请求")
	}
	req = req.WithContext(ctx)
	// Media requests carry no session material at all.
	req.Header.Set("User-Agent", "sph-local/1.0")
	req.Header.Set("Accept", "*/*")
	req.Header.Set("Accept-Encoding", "identity")
	req.Header.Set("Referer", "https://channels.weixin.qq.com/")

	resp, err := d.opts.HTTP.Do(req)
	if err != nil {
		return 0, "", d.mapTransferError(ctx, err)
	}
	defer resp.Body.Close()
	switch {
	case resp.StatusCode == http.StatusOK:
	case resp.StatusCode == http.StatusPartialContent:
		return 0, "", apperr.New(apperr.DownloadFailed, apperr.StageDownload, "媒体服务器返回 206，本工具未请求 Range，拒绝保存不完整响应")
	case resp.StatusCode == http.StatusNotFound:
		return 0, "", apperr.New(apperr.DownloadFailed, apperr.StageDownload, "媒体文件不存在 (HTTP 404)")
	default:
		return 0, "", apperr.Newf(apperr.DownloadFailed, apperr.StageDownload, "媒体下载失败 (HTTP %d)", resp.StatusCode)
	}
	if d.opts.MaxBytes > 0 {
		if cl := resp.ContentLength; cl > d.opts.MaxBytes {
			return 0, "", apperr.Newf(apperr.DownloadTooLarge, apperr.StageDownload,
				"文件大小 %d 超过上限 %d 字节", cl, d.opts.MaxBytes)
		}
	}
	tmp, err := os.CreateTemp(filepath.Dir(d.opts.OutputPath), ".sph-*.part")
	if err != nil {
		return 0, "", apperr.Wrap(err, apperr.IOError, apperr.StageDownload, "无法创建临时文件")
	}
	d.tmpPath = tmp.Name()
	if err := tmp.Chmod(0o600); err != nil {
		tmp.Close()
		return 0, "", apperr.Wrap(err, apperr.IOError, apperr.StageDownload, "无法设置临时文件权限")
	}

	hash := sha256.New()
	sent := int64(0)
	buf := make([]byte, copyBufferBytes)
	first := true
	var sniff [sniffBytes]byte
	for {
		n, readErr := resp.Body.Read(buf)
		if n > 0 {
			chunk := buf[:n]
			if first {
				copy(sniff[:], chunk)
				if err := checkSniff(sniff[:min(n, sniffBytes)]); err != nil {
					tmp.Close()
					return 0, "", err
				}
				first = false
			}
			written, writeErr := tmp.Write(chunk)
			if writeErr != nil {
				tmp.Close()
				return 0, "", apperr.Wrap(writeErr, apperr.IOError, apperr.StageDownload, "写入临时文件失败")
			}
			if int64(written) != int64(n) {
				tmp.Close()
				return 0, "", apperr.New(apperr.IOError, apperr.StageDownload, "写入临时文件不完整")
			}
			hash.Write(chunk)
			sent += int64(n)
			if d.opts.MaxBytes > 0 && sent > d.opts.MaxBytes {
				tmp.Close()
				return 0, "", apperr.Newf(apperr.DownloadTooLarge, apperr.StageDownload,
					"已下载字节超过上限 %d", d.opts.MaxBytes)
			}
			if d.opts.Progress != nil {
				d.opts.Progress(sent, resp.ContentLength)
			}
		}
		if readErr == io.EOF {
			break
		}
		if readErr != nil {
			tmp.Close()
			return 0, "", &apperr.Error{
				Code: apperr.NetworkError, Stage: apperr.StageDownload,
				Message: "媒体传输中断", Retryable: true, Err: readErr,
			}
		}
		if ctx.Err() != nil {
			tmp.Close()
			return 0, "", classifyCancel(ctx, apperr.StageDownload)
		}
	}
	if sent == 0 {
		tmp.Close()
		return 0, "", apperr.New(apperr.DownloadFailed, apperr.StageDownload, "媒体响应为空")
	}
	if resp.ContentLength > 0 && sent != resp.ContentLength {
		tmp.Close()
		return 0, "", apperr.Newf(apperr.DownloadFailed, apperr.StageDownload,
			"下载不完整：收到 %d 字节，Content-Length 为 %d", sent, resp.ContentLength)
	}
	if err := tmp.Sync(); err != nil {
		tmp.Close()
		return 0, "", apperr.Wrap(err, apperr.IOError, apperr.StageDownload, "同步临时文件失败")
	}
	if err := tmp.Close(); err != nil {
		return 0, "", apperr.Wrap(err, apperr.IOError, apperr.StageDownload, "关闭临时文件失败")
	}
	return sent, hex.EncodeToString(hash.Sum(nil)), nil
}

// checkSniff rejects obvious non-video bodies early.
func checkSniff(head []byte) error {
	s := strings.TrimSpace(string(head))
	lower := strings.ToLower(s)
	switch {
	case strings.HasPrefix(lower, "#extm3u"), strings.Contains(lower, "#ext-x-"):
		return apperr.New(apperr.UnsupportedMedia, apperr.StageDownload, "返回的是 HLS 播放列表，本版本不支持")
	case strings.HasPrefix(lower, "<mpd"):
		return apperr.New(apperr.UnsupportedMedia, apperr.StageDownload, "返回的是 DASH 清单，本版本不支持")
	case strings.HasPrefix(lower, "<html"), strings.HasPrefix(lower, "<!doctype html"):
		return apperr.New(apperr.DownloadFailed, apperr.StageDownload, "媒体服务器返回了 HTML 错误页")
	case strings.HasPrefix(lower, "<?xml"):
		return apperr.New(apperr.DownloadFailed, apperr.StageDownload, "媒体服务器返回了 XML 错误响应")
	case strings.HasPrefix(s, "{"), strings.HasPrefix(s, "["):
		return apperr.New(apperr.DownloadFailed, apperr.StageDownload, "媒体服务器返回了 JSON 错误响应")
	}
	return nil
}

// commit verifies the temp file, then atomically publishes it.
func (d *Downloader) commit(ctx context.Context, bytes int64, sum string) (Result, error) {
	method, err := d.opts.Verify(ctx, d.tmpPath)
	if err != nil {
		return Result{}, err
	}
	if err := os.Chmod(d.tmpPath, 0o600); err != nil {
		return Result{}, apperr.Wrap(err, apperr.IOError, apperr.StageCommit, "设置最终权限失败")
	}
	target := d.opts.OutputPath
	if d.opts.Overwrite {
		if err := os.Rename(d.tmpPath, target); err != nil {
			return Result{}, apperr.Wrap(err, apperr.IOError, apperr.StageCommit, "覆盖提交失败，旧文件保持不变")
		}
		d.tmpPath = ""
	} else {
		// hardlink-then-unlink gives atomic no-clobber on the same filesystem;
		// a plain stat+rename could clobber a concurrently created file
		if err := os.Link(d.tmpPath, target); err != nil {
			if errors.Is(err, os.ErrExist) {
				return Result{}, apperr.Newf(apperr.FileExists, apperr.StageCommit,
					"目标文件已存在：%s（使用 --overwrite 覆盖）", target)
			}
			return Result{}, apperr.Wrap(err, apperr.IOError, apperr.StageCommit,
				"当前文件系统不支持原子提交（硬链接），已停止而不写出半个文件")
		}
		if err := os.Remove(d.tmpPath); err != nil && !os.IsNotExist(err) {
			// final file is committed; the leftover hardlink is only a warning
			if d.opts.Warn != nil {
				d.opts.Warn("下载完成，但清理临时硬链接失败，请手动删除 " + d.tmpPath)
			}
		}
		d.tmpPath = ""
	}
	return Result{
		LocalID:      d.video.LocalID,
		Path:         target,
		Bytes:        bytes,
		SHA256:       sum,
		Verification: method,
	}, nil
}

func (d *Downloader) mapTransferError(ctx context.Context, err error) error {
	if ctx.Err() != nil {
		return classifyCancel(ctx, apperr.StageDownload)
	}
	return netpolicy.WrapNetError(err, apperr.StageDownload, "媒体请求失败: ")
}

func classifyCancel(ctx context.Context, stage apperr.Stage) *apperr.Error {
	if errors.Is(ctx.Err(), context.Canceled) {
		return apperr.Wrap(ctx.Err(), apperr.Cancelled, stage, "下载已取消")
	}
	return apperr.Wrap(ctx.Err(), apperr.Timeout, stage, "下载超时")
}

func isRetryableTransfer(err error) bool {
	var ae *apperr.Error
	if errors.As(err, &ae) {
		return ae.Code == apperr.NetworkError || ae.Retryable
	}
	return false
}
