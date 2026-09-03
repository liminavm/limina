// Reports the CoreAudio output latency the virtio-snd device currently discards.
// Build: swiftc -O devlatency.swift -o devlatency
import CoreAudio
import AudioToolbox

func u32(_ dev: AudioObjectID, _ sel: AudioObjectPropertySelector,
         _ scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeOutput) -> UInt32? {
    var addr = AudioObjectPropertyAddress(mSelector: sel, mScope: scope,
                                          mElement: kAudioObjectPropertyElementMain)
    var v: UInt32 = 0
    var sz = UInt32(MemoryLayout<UInt32>.size)
    return AudioObjectGetPropertyData(dev, &addr, 0, nil, &sz, &v) == noErr ? v : nil
}

var addr = AudioObjectPropertyAddress(mSelector: kAudioHardwarePropertyDefaultOutputDevice,
                                      mScope: kAudioObjectPropertyScopeGlobal,
                                      mElement: kAudioObjectPropertyElementMain)
var dev = AudioObjectID(0)
var sz = UInt32(MemoryLayout<AudioObjectID>.size)
guard AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &addr, 0, nil, &sz, &dev) == noErr
else { fputs("no default output device\n", stderr); exit(1) }

var nameAddr = AudioObjectPropertyAddress(mSelector: kAudioObjectPropertyName,
                                          mScope: kAudioObjectPropertyScopeGlobal,
                                          mElement: kAudioObjectPropertyElementMain)
// The HAL writes a retained CFStringRef; take it through an opaque pointer so Swift
// never sees a managed reference it did not create.
var namePtr: UnsafeMutableRawPointer? = nil
var nsz = UInt32(MemoryLayout<UnsafeMutableRawPointer?>.size)
_ = AudioObjectGetPropertyData(dev, &nameAddr, 0, nil, &nsz, &namePtr)
let name = namePtr.map { Unmanaged<CFString>.fromOpaque($0).takeRetainedValue() as String } ?? "unknown"

var rateAddr = AudioObjectPropertyAddress(mSelector: kAudioDevicePropertyNominalSampleRate,
                                          mScope: kAudioObjectPropertyScopeOutput,
                                          mElement: kAudioObjectPropertyElementMain)
var rate: Float64 = 0
var rsz = UInt32(MemoryLayout<Float64>.size)
_ = AudioObjectGetPropertyData(dev, &rateAddr, 0, nil, &rsz, &rate)

// The stream's own latency sits on the first output stream, not the device.
var strAddr = AudioObjectPropertyAddress(mSelector: kAudioDevicePropertyStreams,
                                         mScope: kAudioObjectPropertyScopeOutput,
                                         mElement: kAudioObjectPropertyElementMain)
var streamLatency: UInt32 = 0
var ssz: UInt32 = 0
if AudioObjectGetPropertyDataSize(dev, &strAddr, 0, nil, &ssz) == noErr, ssz > 0 {
    var streams = [AudioStreamID](repeating: 0, count: Int(ssz) / MemoryLayout<AudioStreamID>.size)
    if AudioObjectGetPropertyData(dev, &strAddr, 0, nil, &ssz, &streams) == noErr, let first = streams.first {
        streamLatency = u32(first, kAudioStreamPropertyLatency, kAudioObjectPropertyScopeGlobal) ?? 0
    }
}

let devLatency = u32(dev, kAudioDevicePropertyLatency) ?? 0
let safety = u32(dev, kAudioDevicePropertySafetyOffset) ?? 0
let buffer = u32(dev, kAudioDevicePropertyBufferFrameSize) ?? 0
let total = devLatency + safety + buffer + streamLatency
let r = rate > 0 ? rate : 48000

print("device:          \(name)")
print("sample rate:     \(r)")
print("device latency:  \(devLatency) frames")
print("stream latency:  \(streamLatency) frames")
print("safety offset:   \(safety) frames")
print("buffer size:     \(buffer) frames")
print(String(format: "TOTAL:           %u frames = %.1f ms", total, Double(total) * 1000.0 / r))
