// What can VideoToolbox decode in hardware on THIS Mac?
//
// The host half of the VA-API-over-virgl question: a virglrenderer VideoToolbox
// backend can only advertise `virgl_video_caps` for codecs the running host
// actually has silicon for, and that set differs per Apple GPU generation
// (AV1 needs M3+). Run it on the machine you are about to make claims about.
//
//   swift vt-caps.swift                          # this Mac
//   ssh <mac> 'swift -' < vt-caps.swift          # another Mac, writes nothing there
//
// VP9 and AV1 need VTRegisterSupplementalVideoDecoderIfAvailable() first;
// without it VTIsHardwareDecodeSupported reports them as unsupported even on
// silicon that has them.

import CoreMedia
import VideoToolbox

let codecs: [(String, CMVideoCodecType)] = [
    ("H.264", kCMVideoCodecType_H264),
    ("HEVC", kCMVideoCodecType_HEVC),
    ("VP9", kCMVideoCodecType_VP9),
    ("AV1", kCMVideoCodecType_AV1),
    ("MPEG-2", kCMVideoCodecType_MPEG2Video),
    ("MPEG-4", kCMVideoCodecType_MPEG4Video),
    ("MJPEG", kCMVideoCodecType_JPEG),
    ("ProRes 422", kCMVideoCodecType_AppleProRes422),
]

VTRegisterSupplementalVideoDecoderIfAvailable(kCMVideoCodecType_VP9)
VTRegisterSupplementalVideoDecoderIfAvailable(kCMVideoCodecType_AV1)

for (name, codec) in codecs {
    let pad = String(repeating: " ", count: max(0, 12 - name.count))
    print("\(name)\(pad)hwdec = \(VTIsHardwareDecodeSupported(codec) ? "YES" : "no")")
}
