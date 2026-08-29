// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva
//
// now-playing-media-keys — can limina claim macOS's Now Playing session, with no
// metadata and no audio of its own, and does fn+F8 then come back as a remote command?
//
// The design under test routes media keys through macOS's media-session arbitration
// instead of the aux-key bucket policy: the VM announces itself as a player while the
// guest holds the virtio-snd stream open, and MPRemoteCommandCenter callbacks are
// translated back into evdev media keys for the guest. Two premises need measuring
// before any of that gets built:
//
//   1. Does a process with NO audio output of its own get Now Playing status? In limina
//      the CoreAudio sink lives in libkrun, in the *worker* process, while MediaPlayer
//      has to live in the AppKit process. If macOS ties eligibility to the registering
//      process actually producing audio, the design has a fork.
//   2. Is a title-only info dict (no duration, no position, no artwork) enough, or does
//      macOS want a scrubbable track before it will route keys to us?
//
// Arms, via flags:
//   (none)     title-only info dict, playbackState = .playing, no audio at all
//   --audio    same, plus a near-silent AVAudioEngine output in *this* process, to
//              isolate "must produce audio" from "must merely register"
//   --paused   register as .paused instead, for the retire/contention arm
//   --no-info  register commands but never set nowPlayingInfo, to see whether the
//              command center alone is enough
//   --audio-only     render the tone and nothing else: no MediaPlayer, no registration.
//                    This is the worker's half of limina, run as its own process.
//   --audio-sibling  register here, but spawn a --audio-only CHILD to do the rendering.
//                    This is limina's actual shape, and the arm that decides whether the
//                    session registration can stay in the supervisor.
//
// The premise both of those test: macOS's arbitration is sticky to whoever most recently
// RENDERED audio (measured, arms 5-6), and limina's registering process renders nothing.
// If credit does not reach across the spawn, a limina that never renders can never take
// its turn, however correctly it registers.
//
// State is drivable at runtime over stdin — `playing` / `paused` / `stopped` / `clear`
// / `publish` / `disable` / `unwire` / `rewire` — so the retire and contention arms (does releasing the session hand keys
// back? does re-registering steal them from Music?) can be run against one long-lived
// process, with the human free to press the key at each step.
//
// Runs as an .accessory app: never frontmost, so a delivered key proves system-wide
// media-session routing and not mere key-window focus.

import AppKit
import AVFoundation
import MediaPlayer

let args = Set(CommandLine.arguments.dropFirst())
let wantAudio = args.contains("--audio")
let wantPaused = args.contains("--paused")
let noInfo = args.contains("--no-info")
// The ranking arms. --audio-only is the worker: it renders and registers nothing.
// --audio-sibling is limina's real topology: this process registers with MediaPlayer and
// *spawns* a child that does the rendering, so the child's responsible process is us —
// exactly as the supervisor spawns the worker. Launching the tone from a separate shell
// instead would make the terminal responsible and would not model limina at all.
let audioOnly = args.contains("--audio-only")
let audioSibling = args.contains("--audio-sibling")

let started = Date()
func log(_ msg: String) {
    let t = String(format: "%7.3f", Date().timeIntervalSince(started))
    print("[\(t)] \(msg)")
    fflush(stdout)
}

// MARK: - near-silent output, for the --audio arm

// Real amplitude, not digital silence: a sink that renders exact zeros is a plausible
// thing for macOS to treat as "not playing", and the point of this arm is to give the
// process an unambiguously live CoreAudio unit. -80 dBFS is inaudible but nonzero.
final class Silence {
    private let engine = AVAudioEngine()

    func start() {
        let out = engine.outputNode
        let fmt = out.inputFormat(forBus: 0)
        var phase: Float = 0
        let step = 2 * Float.pi * 440 / Float(fmt.sampleRate)
        let src = AVAudioSourceNode { _, _, frames, abl in
            let bufs = UnsafeMutableAudioBufferListPointer(abl)
            for f in 0..<Int(frames) {
                let s = sin(phase) * 1e-4
                phase += step
                if phase > 2 * .pi { phase -= 2 * .pi }
                for b in bufs {
                    b.mData?.assumingMemoryBound(to: Float.self)[f] = s
                }
            }
            return noErr
        }
        engine.attach(src)
        engine.connect(src, to: engine.mainMixerNode, format: fmt)
        do {
            try engine.start()
            log("audio: AVAudioEngine started (\(fmt.sampleRate) Hz, \(fmt.channelCount) ch, -80 dBFS tone)")
        } catch {
            log("audio: FAILED to start engine: \(error)")
        }
    }
}

let silence = Silence()

// MARK: - the media session

func describe(_ state: MPNowPlayingPlaybackState) -> String {
    switch state {
    case .unknown: return "unknown"
    case .playing: return "playing"
    case .paused: return "paused"
    case .stopped: return "stopped"
    case .interrupted: return "interrupted"
    @unknown default: return "?"
    }
}

/// Every command we would wire to the guest, plus the ones we must *disable* so Control
/// Center does not render dead buttons. `isEnabled = false` is the documented way to say
/// "this player cannot do that"; leaving them at their default advertises capabilities
/// the guest has no key for.
/// The commands the guest can actually service, i.e. the ones with an evdev key.
func guestCommands() -> [(MPRemoteCommand, String)] {
    let c = MPRemoteCommandCenter.shared()
    return [
        (c.togglePlayPauseCommand, "togglePlayPause"),
        (c.playCommand, "play"),
        (c.pauseCommand, "pause"),
        (c.stopCommand, "stop"),
        (c.nextTrackCommand, "nextTrack"),
        (c.previousTrackCommand, "previousTrack"),
    ]
}

func registerCommands() {
    let c = MPRemoteCommandCenter.shared()

    func wire(_ cmd: MPRemoteCommand, _ name: String) {
        cmd.isEnabled = true
        cmd.addTarget { ev in
            log(">>> REMOTE COMMAND: \(name)  (\(type(of: ev)))")
            return .success
        }
    }
    for (cmd, name) in guestCommands() { wire(cmd, name) }

    for (cmd, name) in [
        (c.changePlaybackPositionCommand, "changePlaybackPosition"),
        (c.seekForwardCommand, "seekForward"),
        (c.seekBackwardCommand, "seekBackward"),
        (c.skipForwardCommand, "skipForward"),
        (c.skipBackwardCommand, "skipBackward"),
        (c.changeShuffleModeCommand, "changeShuffleMode"),
        (c.changeRepeatModeCommand, "changeRepeatMode"),
        (c.likeCommand, "like"),
        (c.dislikeCommand, "dislike"),
        (c.bookmarkCommand, "bookmark"),
        (c.ratingCommand, "rating"),
        (c.changePlaybackRateCommand, "changePlaybackRate"),
    ] as [(MPRemoteCommand, String)] {
        cmd.isEnabled = false
        _ = name
    }
    log("commands: registered (toggle/play/pause/stop/next/prev enabled, rest disabled)")
}

func publish() {
    let center = MPNowPlayingInfoCenter.default()
    if noInfo {
        log("info: --no-info, leaving nowPlayingInfo nil")
    } else {
        // Deliberately minimal: exactly what limina would know without a guest agent.
        center.nowPlayingInfo = [
            MPMediaItemPropertyTitle: "Fedora 44 (limina)",
        ]
        log("info: published title-only dict (no duration/elapsed/rate/artwork)")
    }
    center.playbackState = wantPaused ? .paused : .playing
    log("info: playbackState = \(describe(center.playbackState))")
}

/// Runtime state control, one word per line on stdin. Reading happens off the main
/// thread and hops back, because MediaPlayer wants its main-thread runloop.
func startStdinControl() {
    Thread.detachNewThread {
        while let line = readLine(strippingNewline: true) {
            let word = line.trimmingCharacters(in: .whitespaces)
            guard !word.isEmpty else { continue }
            DispatchQueue.main.async {
                let center = MPNowPlayingInfoCenter.default()
                switch word {
                case "playing": center.playbackState = .playing
                case "paused": center.playbackState = .paused
                case "stopped": center.playbackState = .stopped
                case "clear": center.nowPlayingInfo = nil
                case "publish": publish(); return
                case "disable":
                    for (cmd, _) in guestCommands() { cmd.isEnabled = false }
                    log("stdin: disable -> all guest commands isEnabled=false")
                    return
                case "unwire":
                    for (cmd, _) in guestCommands() { cmd.removeTarget(nil); cmd.isEnabled = false }
                    log("stdin: unwire -> all guest commands removeTarget(nil) + disabled")
                    return
                case "rewire": registerCommands(); return
                default:
                    log("stdin: unknown command \(word)")
                    return
                }
                log("stdin: \(word) -> playbackState=\(describe(center.playbackState)) info=\(center.nowPlayingInfo == nil ? "nil" : "set")")
            }
        }
    }
}

// MARK: - the sibling renderer

/// Spawn ourselves with --audio-only, so the tone is rendered by a *child* of the process
/// that holds the media session. Kept alive for the run and killed with us.
var audioChild: Process?
func spawnAudioChild() {
    let p = Process()
    p.executableURL = URL(fileURLWithPath: CommandLine.arguments[0])
    p.arguments = ["--audio-only"]
    do {
        try p.run()
        audioChild = p
        log("audio-sibling: spawned child pid=\(p.processIdentifier) rendering the tone")
    } catch {
        log("audio-sibling: FAILED to spawn child: \(error)")
    }
}

// MARK: - main

final class Delegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_: Notification) {
        let b = Bundle.main
        log("bundle: id=\(b.bundleIdentifier ?? "<none>") path=\(b.bundlePath)")
        if wantAudio || audioOnly { silence.start() }
        if audioOnly {
            log("audio-only: rendering, NOT registering — this process is the worker's stand-in.")
            // Reap ourselves when the parent goes away, so no tone outlives the arm.
            Thread.detachNewThread {
                while getppid() != 1 { Thread.sleep(forTimeInterval: 0.5) }
                exit(0)
            }
            return
        }
        if audioSibling { spawnAudioChild() }
        registerCommands()
        publish()
        startStdinControl()
        log("ready — press fn+F8 (and the Control Center transport buttons). Ctrl-C to stop.")
        log("       this process is .accessory, so it is never frontmost: a delivered")
        log("       command proves system-wide media-session routing, not focus.")
    }
}

let app = NSApplication.shared
let delegate = Delegate()
app.delegate = delegate
app.setActivationPolicy(.accessory)
app.run()
