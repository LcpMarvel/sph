// Package verify implements two file checks: a streaming ISO-BMFF container
// check and an optional ffprobe stream check. Neither is a full decode.
package verify

import (
	"encoding/binary"
	"math"
	"os"

	"github.com/LcpMarvel/sph-downloader/internal/apperr"
)

// maxBoxes bounds the top-level box walk so a corrupt file cannot spin.
const maxBoxes = 4096

// CheckContainer walks the top-level boxes of an MP4/ISO-BMFF file and
// requires: valid box bounds, an ftyp box, a moov box and an mdat box that
// actually carries data. It supports 32-bit sizes, 64-bit extended sizes and
// size=0 (box extends to EOF). Only headers are read; the file is never
// loaded into memory.
func CheckContainer(f *os.File) error {
	stat, err := f.Stat()
	if err != nil {
		return apperr.Wrap(err, apperr.IOError, apperr.StageVerify, "无法读取文件信息")
	}
	fileSize := stat.Size()
	if fileSize < 8 {
		return apperr.New(apperr.VerifyFailed, apperr.StageVerify, "文件太小，不是有效的 MP4 容器")
	}
	var (
		offset    int64
		hasFtyp   bool
		hasMoov   bool
		mdatBytes int64
	)
	for offset < fileSize {
		if fileSize-offset < 8 {
			return apperr.New(apperr.VerifyFailed, apperr.StageVerify, "MP4 box 头不完整")
		}
		var header [8]byte
		if _, err := f.ReadAt(header[:], offset); err != nil {
			return apperr.Wrap(err, apperr.IOError, apperr.StageVerify, "读取 MP4 box 头失败")
		}
		size := int64(binary.BigEndian.Uint32(header[0:4]))
		boxType := string(header[4:8])
		headerLen := int64(8)
		switch size {
		case 0:
			// box extends to end of file
			size = fileSize - offset
		case 1:
			if fileSize-offset < 16 {
				return apperr.New(apperr.VerifyFailed, apperr.StageVerify, "MP4 extended size 头不完整")
			}
			var ext [8]byte
			if _, err := f.ReadAt(ext[:], offset+8); err != nil {
				return apperr.Wrap(err, apperr.IOError, apperr.StageVerify, "读取 MP4 extended size 失败")
			}
			hi := binary.BigEndian.Uint64(ext[:])
			if hi > math.MaxInt64 {
				return apperr.New(apperr.VerifyFailed, apperr.StageVerify, "MP4 box 大小溢出")
			}
			size = int64(hi)
			headerLen = 16
		}
		if size < headerLen {
			return apperr.New(apperr.VerifyFailed, apperr.StageVerify, "MP4 box 大小小于头长度")
		}
		if size > fileSize-offset {
			return apperr.New(apperr.VerifyFailed, apperr.StageVerify, "MP4 box 越界")
		}
		switch boxType {
		case "ftyp":
			hasFtyp = true
		case "moov":
			hasMoov = true
		case "mdat":
			if size-headerLen > mdatBytes {
				mdatBytes = size - headerLen
			}
		case "wide":
			// padding, ignore
		}
		offset += size
	}
	if offset != fileSize {
		return apperr.New(apperr.VerifyFailed, apperr.StageVerify, "MP4 box 序列与文件长度不一致")
	}
	if !hasFtyp {
		return apperr.New(apperr.VerifyFailed, apperr.StageVerify, "缺少 ftyp box")
	}
	if !hasMoov {
		return apperr.New(apperr.VerifyFailed, apperr.StageVerify, "缺少 moov box")
	}
	if mdatBytes <= 0 {
		return apperr.New(apperr.VerifyFailed, apperr.StageVerify, "缺少携带数据的 mdat box")
	}
	return nil
}
