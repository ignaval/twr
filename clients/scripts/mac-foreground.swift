// Native QA evidence without Accessibility/Screen Recording or production IPC hooks.
import AppKit
import CoreGraphics
import Foundation

let targetPID = pid_t(CommandLine.arguments.dropFirst().first ?? "0") ?? 0
let target = NSRunningApplication(processIdentifier: targetPID)
let requested = CommandLine.arguments.contains("--activate")
var activated = false
let loginWindow = NSWorkspace.shared.frontmostApplication?.bundleIdentifier == "com.apple.loginwindow"
if requested && !loginWindow {
    activated = target?.activate(options: [.activateAllWindows]) ?? false
    RunLoop.current.run(until: Date().addingTimeInterval(0.3))
}
let windows = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
func owner(_ row: [String: Any]) -> pid_t { (row[kCGWindowOwnerPID as String] as? NSNumber)?.int32Value ?? 0 }
func layer(_ row: [String: Any]) -> Int { (row[kCGWindowLayer as String] as? NSNumber)?.intValue ?? -1 }
func bounds(_ row: [String: Any]) -> CGRect? {
    guard let value = row[kCGWindowBounds as String] as? [String: Any] else { return nil }
    return CGRect(dictionaryRepresentation: value as CFDictionary)
}
let own = windows.enumerated().first { owner($0.element) == targetPID && layer($0.element) == 0 }
let frame = own.flatMap { bounds($0.element) }
let overlapping = own.map { window in
    windows.prefix(window.offset).filter { row in
        guard owner(row) != targetPID, layer(row) == 0, let other = bounds(row), let frame = frame else { return false }
        return other.intersects(frame)
    }.count
} ?? 0
let result: [String: Any] = [
    "targetPID": targetPID,
    "frontmostPID": NSWorkspace.shared.frontmostApplication?.processIdentifier ?? 0,
    "frontmostIsLoginWindow": NSWorkspace.shared.frontmostApplication?.bundleIdentifier == "com.apple.loginwindow",
    "activationSkippedForLock": requested && loginWindow,
    "targetActive": target?.isActive ?? false,
    "targetOnScreen": own != nil,
    "overlappingHigherWindows": overlapping,
    "activationRequested": requested,
    "activationSucceeded": activated,
    "windowBounds": frame.map { ["x": $0.minX, "y": $0.minY, "width": $0.width, "height": $0.height] } ?? [:],
]
let data = try JSONSerialization.data(withJSONObject: result, options: [.sortedKeys])
print(String(data: data, encoding: .utf8)!)
