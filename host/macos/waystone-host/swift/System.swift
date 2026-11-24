// Display geometry, speaker mute and screen lock: the parts of a session's
// setup and rollback that touch the system (design doc 4.4 and 4.5).

import CoreAudio
import CoreGraphics
import Foundation

@_cdecl("ws_display_main")
public func ws_display_main() -> UInt32 {
    CGMainDisplayID()
}

/// Bounds in global points (what CGEvent positions use) and the native pixel
/// size of the panel (what capture can resolve).
@_cdecl("ws_display_info")
public func ws_display_info(
    _ display: UInt32, _ x: UnsafeMutablePointer<Double>, _ y: UnsafeMutablePointer<Double>,
    _ width: UnsafeMutablePointer<Double>, _ height: UnsafeMutablePointer<Double>,
    _ pixelWidth: UnsafeMutablePointer<Int32>, _ pixelHeight: UnsafeMutablePointer<Int32>
) -> Bool {
    let bounds = CGDisplayBounds(display)
    guard bounds.width > 0, bounds.height > 0 else { return false }
    x.pointee = bounds.origin.x
    y.pointee = bounds.origin.y
    width.pointee = bounds.width
    height.pointee = bounds.height
    let mode = CGDisplayCopyDisplayMode(display)
    pixelWidth.pointee = Int32(mode?.pixelWidth ?? Int(bounds.width))
    pixelHeight.pointee = Int32(mode?.pixelHeight ?? Int(bounds.height))
    return true
}

// MARK: - Screen lock

/// Locks the session immediately, like Ctrl-Cmd-Q.
///
/// `SACLockScreenImmediate` in the login framework is what the menu item
/// calls; it is private, so it is looked up at run time and the agent falls
/// back to the keyboard shortcut if it ever disappears.
@_cdecl("ws_lock_screen")
public func ws_lock_screen() -> Bool {
    let path = "/System/Library/PrivateFrameworks/login.framework/Versions/Current/login"
    if let handle = dlopen(path, RTLD_LAZY), let symbol = dlsym(handle, "SACLockScreenImmediate") {
        typealias Lock = @convention(c) () -> Int32
        let lock = unsafeBitCast(symbol, to: Lock.self)
        if lock() == 0 { return true }
    }
    // kVK_ANSI_Q with Control and Command.
    let source = CGEventSource(stateID: .hidSystemState)
    for down in [true, false] {
        let event = CGEvent(keyboardEventSource: source, virtualKey: 0x0C, keyDown: down)
        event?.flags = [.maskControl, .maskCommand]
        event?.post(tap: .cghidEventTap)
    }
    return true
}

// MARK: - Speaker mute

private func defaultOutputDevice() -> AudioDeviceID? {
    var device = AudioDeviceID(0)
    var size = UInt32(MemoryLayout<AudioDeviceID>.size)
    var address = AudioObjectPropertyAddress(
        mSelector: kAudioHardwarePropertyDefaultOutputDevice, mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain)
    let status = AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &address, 0, nil, &size, &device)
    return status == noErr && device != 0 ? device : nil
}

private var muteAddress = AudioObjectPropertyAddress(
    mSelector: kAudioDevicePropertyMute, mScope: kAudioDevicePropertyScopeOutput,
    mElement: kAudioObjectPropertyElementMain)

/// 1 muted, 0 not muted, -1 when the output device has no mute control.
@_cdecl("ws_output_muted")
public func ws_output_muted() -> Int32 {
    guard let device = defaultOutputDevice(), AudioObjectHasProperty(device, &muteAddress) else { return -1 }
    var muted: UInt32 = 0
    var size = UInt32(MemoryLayout<UInt32>.size)
    guard AudioObjectGetPropertyData(device, &muteAddress, 0, nil, &size, &muted) == noErr else { return -1 }
    return muted == 0 ? 0 : 1
}

/// Mutes the speakers. ScreenCaptureKit captures application audio before the
/// device, so the stream keeps its sound.
@_cdecl("ws_set_output_muted")
public func ws_set_output_muted(_ muted: Bool) -> Bool {
    guard let device = defaultOutputDevice(), AudioObjectHasProperty(device, &muteAddress) else { return false }
    var value: UInt32 = muted ? 1 : 0
    return AudioObjectSetPropertyData(device, &muteAddress, 0, nil, UInt32(MemoryLayout<UInt32>.size), &value) == noErr
}
