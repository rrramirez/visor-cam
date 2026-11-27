// Drives a Visor test instance for the ui-test skill: mouse, keyboard and
// window queries through CoreGraphics. Needs the Accessibility permission
// for the app hosting the session (see SKILL.md). Coordinates are screen
// points, top-left origin, as `windows` prints them.
//
//   trusted                        -> "true" / "false"
//   prompt                         -> asks macOS to show the permission request
//   windows <pid>                  -> "<id> <x> <y> <w> <h> <layer> <title>" per window
//   front <pid>                    -> brings that app to the front
//   click <x> <y> [shift|cmd]      -> left click (with a modifier held)
//   drag <x1> <y1> <x2> <y2>       -> press, move slowly, release (starts drag sessions too)
//   scroll <x> <y> <dy>            -> scroll by dy pixels (positive = up)
//   key <keycode> [cmd,shift,alt]  -> key press, with real modifier presses around it
//   type <text>                    -> types text
import AppKit
import ApplicationServices
import CoreGraphics

let a = CommandLine.arguments
guard a.count > 1 else { exit(2) }
let src = CGEventSource(stateID: .hidSystemState)
var clickFlags: CGEventFlags = []

func mouse(_ t: CGEventType, _ x: Double, _ y: Double) {
    let e = CGEvent(mouseEventSource: src, mouseType: t, mouseCursorPosition: CGPoint(x: x, y: y), mouseButton: .left)
    e?.flags = clickFlags
    e?.post(tap: .cghidEventTap)
}

func post(_ code: CGKeyCode, down: Bool, _ flags: CGEventFlags) {
    let e = CGEvent(keyboardEventSource: src, virtualKey: code, keyDown: down)
    e?.flags = flags
    e?.post(tap: .cghidEventTap)
    usleep(30_000)
}

func num(_ i: Int) -> Double { Double(a[i])! }

switch a[1] {
case "trusted":
    print(AXIsProcessTrusted())
case "prompt":
    let opts = [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true] as CFDictionary
    print(AXIsProcessTrustedWithOptions(opts))
case "windows":
    let pid = Int(a[2])!
    let list = CGWindowListCopyWindowInfo([.optionAll], kCGNullWindowID) as? [[String: Any]] ?? []
    for w in list where (w[kCGWindowOwnerPID as String] as? Int) == pid {
        guard let id = w[kCGWindowNumber as String] as? Int,
              let b = w[kCGWindowBounds as String] as? [String: Double],
              let layer = w[kCGWindowLayer as String] as? Int else { continue }
        let title = w[kCGWindowName as String] as? String ?? ""
        if b["Width"]! < 2 { continue }
        print(id, Int(b["X"]!), Int(b["Y"]!), Int(b["Width"]!), Int(b["Height"]!), layer, title)
    }
case "front":
    let ok = NSRunningApplication(processIdentifier: pid_t(Int32(a[2])!))?.activate(options: [.activateAllWindows]) ?? false
    usleep(400_000)
    print(ok)
case "click":
    if a.count > 4 { clickFlags = a[4] == "cmd" ? .maskCommand : a[4] == "shift" ? .maskShift : [] }
    mouse(.mouseMoved, num(2), num(3)); usleep(80_000)
    mouse(.leftMouseDown, num(2), num(3)); usleep(60_000)
    mouse(.leftMouseUp, num(2), num(3))
case "drag":
    let (x1, y1, x2, y2) = (num(2), num(3), num(4), num(5))
    mouse(.mouseMoved, x1, y1); usleep(100_000)
    mouse(.leftMouseDown, x1, y1); usleep(150_000)
    // A few small moves first: AppKit starts a drag session on them.
    for i in 1...5 { mouse(.leftMouseDragged, x1 + Double(i) * 2, y1); usleep(60_000) }
    for i in 1...60 {
        let f = Double(i) / 60
        mouse(.leftMouseDragged, x1 + 10 + (x2 - x1 - 10) * f, y1 + (y2 - y1) * f); usleep(30_000)
    }
    for _ in 1...10 { mouse(.leftMouseDragged, x2, y2); usleep(50_000) }
    mouse(.leftMouseUp, x2, y2)
case "scroll":
    let dy = Int32(a[4])!
    mouse(.mouseMoved, num(2), num(3)); usleep(100_000)
    for _ in 0..<10 {
        CGEvent(scrollWheelEvent2Source: src, units: .pixel, wheelCount: 1, wheel1: dy / 10, wheel2: 0, wheel3: 0)?
            .post(tap: .cghidEventTap)
        usleep(20_000)
    }
case "key":
    let code = CGKeyCode(Int(a[2])!)
    var mods: [(CGKeyCode, CGEventFlags)] = []
    for m in (a.count > 3 ? a[3] : "").split(separator: ",") {
        switch m {
        case "cmd": mods.append((55, .maskCommand))
        case "shift": mods.append((56, .maskShift))
        case "alt": mods.append((58, .maskAlternate))
        default: break
        }
    }
    // Apps read the modifiers from their own key events, so press them.
    var held: CGEventFlags = []
    for (k, f) in mods { held.insert(f); post(k, down: true, held) }
    post(code, down: true, held)
    post(code, down: false, held)
    for (k, f) in mods.reversed() { held.remove(f); post(k, down: false, held) }
case "type":
    for ch in a[2].utf16 {
        var c = ch
        for down in [true, false] {
            let e = CGEvent(keyboardEventSource: src, virtualKey: 0, keyDown: down)
            e?.keyboardSetUnicodeString(stringLength: 1, unicodeString: &c)
            e?.post(tap: .cghidEventTap)
        }
        usleep(15_000)
    }
default:
    FileHandle.standardError.write("unknown command \(a[1])\n".data(using: .utf8)!)
    exit(2)
}
