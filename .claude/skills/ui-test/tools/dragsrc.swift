// dragsrc FILE...: a small window titled "Visor drag source"; dragging from
// it drags those files.
import AppKit
let files = CommandLine.arguments.dropFirst().map { URL(fileURLWithPath: $0) }
class Source: NSView, NSDraggingSource {
    override func draw(_ r: NSRect) { NSColor.systemTeal.setFill(); r.fill() }
    func draggingSession(_ s: NSDraggingSession, sourceOperationMaskFor c: NSDraggingContext) -> NSDragOperation { .copy }
    override func mouseDown(with e: NSEvent) {}
    override func mouseDragged(with e: NSEvent) {
        let items = files.map { url -> NSDraggingItem in
            let item = NSDraggingItem(pasteboardWriter: url as NSURL)
            item.setDraggingFrame(NSRect(x: 0, y: 0, width: 40, height: 40), contents: nil)
            return item
        }
        beginDraggingSession(with: items, event: e, source: self)
    }
}
let app = NSApplication.shared
app.setActivationPolicy(.regular)
let w = NSWindow(contentRect: NSRect(x: 40, y: 40, width: 160, height: 120), styleMask: [.titled], backing: .buffered, defer: false)
w.title = "Visor drag source"
w.contentView = Source()
w.makeKeyAndOrderFront(nil)
app.activate(ignoringOtherApps: true)
app.run()
