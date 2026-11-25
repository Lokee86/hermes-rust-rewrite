import CoreGraphics
import Foundation
let pid = Int(CommandLine.arguments[1])!
for _ in 0..<60 {
    let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as? [[String: Any]] ?? []
    for window in list where window[kCGWindowOwnerPID as String] as? Int == pid && window[kCGWindowLayer as String] as? Int == 0 {
        let bounds = window[kCGWindowBounds as String] as? [String: Double] ?? [:]
        if (bounds["Width"] ?? 0) > 300 && (bounds["Height"] ?? 0) > 300 {
            print("window \(bounds["Width"]!)x\(bounds["Height"]!)")
            Thread.sleep(forTimeInterval: 5)
            exit(kill(pid_t(pid), 0) == 0 ? 0 : 1)
        }
    }
    Thread.sleep(forTimeInterval: 1)
}
print("No window appeared")
exit(1)
