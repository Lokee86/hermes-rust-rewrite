// Desktop app benchmark for macOS: time to first window, memory footprint, idle CPU and
// process count, for an app and every process it starts. Needs no special permissions.
//
//   swift benchmarks/bench.swift <label> <runs> <executable> [args...]
//
// Environment for the app (data directories) is inherited from this process.
// Prints one JSON object per run.
import AppKit
import Foundation

func processTable() -> [(pid: Int32, ppid: Int32, cpu: Double)] {
    let ps = Process()
    ps.executableURL = URL(fileURLWithPath: "/bin/ps")
    ps.arguments = ["-axo", "pid=,ppid=,time="]
    let pipe = Pipe()
    ps.standardOutput = pipe
    try? ps.run()
    let data = pipe.fileHandleForReading.readDataToEndOfFile()
    ps.waitUntilExit()
    return String(decoding: data, as: UTF8.self).split(separator: "\n").compactMap { line in
        let f = line.split(separator: " ", omittingEmptySubsequences: true)
        guard f.count >= 3, let pid = Int32(f[0]), let ppid = Int32(f[1]) else { return nil }
        // [[dd-]hh:]mm:ss.cc
        var seconds = 0.0
        let time = String(f[2]).replacingOccurrences(of: "-", with: ":")
        for part in time.split(separator: ":") { seconds = seconds * 60 + (Double(part) ?? 0) }
        return (pid, ppid, seconds)
    }
}

func tree(_ root: Int32) -> [(pid: Int32, cpu: Double)] {
    let table = processTable()
    var found: Set<Int32> = [root]
    var grew = true
    while grew {
        grew = false
        for row in table where !found.contains(row.pid) && found.contains(row.ppid) { found.insert(row.pid); grew = true }
    }
    return table.filter { found.contains($0.pid) }.map { ($0.pid, $0.cpu) }
}

/// Physical footprint in bytes per process, from /usr/bin/footprint's JSON.
func footprint(_ pids: [Int32]) -> Double {
    let out = FileManager.default.temporaryDirectory.appendingPathComponent("fp-\(getpid()).json")
    let task = Process()
    task.executableURL = URL(fileURLWithPath: "/usr/bin/footprint")
    task.arguments = ["-j", out.path] + pids.flatMap { ["-p", String($0)] }
    task.standardOutput = FileHandle.nullDevice
    task.standardError = FileHandle.nullDevice
    try? task.run()
    task.waitUntilExit()
    guard let data = try? Data(contentsOf: out),
          let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
          let processes = json["processes"] as? [[String: Any]] else { return -1 }
    try? FileManager.default.removeItem(at: out)
    return processes.reduce(0.0) { $0 + (($1["footprint"] as? Double) ?? Double(($1["footprint"] as? Int) ?? 0)) }
}

func firstWindow(_ pids: Set<Int32>) -> Bool {
    guard let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as? [[String: Any]] else { return false }
    return list.contains { w in
        guard let owner = w[kCGWindowOwnerPID as String] as? Int32, pids.contains(owner) else { return false }
        let layer = w[kCGWindowLayer as String] as? Int ?? 1
        let bounds = w[kCGWindowBounds as String] as? [String: Double] ?? [:]
        return layer == 0 && (bounds["Width"] ?? 0) > 100 && (bounds["Height"] ?? 0) > 100
    }
}

let args = CommandLine.arguments
guard args.count >= 4, let runs = Int(args[2]) else { print("usage: bench.swift <label> <runs> <executable> [args...]"); exit(2) }
let label = args[1]
for run in 1...runs {
    let app = Process()
    app.executableURL = URL(fileURLWithPath: args[3])
    app.arguments = Array(args.dropFirst(4))
    app.standardOutput = FileHandle.nullDevice
    app.standardError = FileHandle.nullDevice
    let start = Date()
    try app.run()
    var windowAt: Double = -1
    while Date().timeIntervalSince(start) < 60 {
        if firstWindow(Set(tree(app.processIdentifier).map { $0.pid })) { windowAt = Date().timeIntervalSince(start); break }
        usleep(5_000)
    }
    // Settle, then measure memory and idle CPU over 10 s.
    while Date().timeIntervalSince(start) < 15 { usleep(100_000) }
    let before = tree(app.processIdentifier)
    let memory = footprint(before.map { $0.pid })
    sleep(10)
    let after = tree(app.processIdentifier)
    let cpuBefore = Dictionary(uniqueKeysWithValues: before.map { ($0.pid, $0.cpu) })
    let cpuSeconds = after.reduce(0.0) { $0 + max(0, $1.cpu - (cpuBefore[$1.pid] ?? 0)) }
    let processes = after.count
    for p in after.reversed() { kill(p.pid, SIGTERM) }
    sleep(2)
    for p in tree(app.processIdentifier) { kill(p.pid, SIGKILL) }
    kill(app.processIdentifier, SIGKILL)
    app.waitUntilExit()
    let line: [String: Any] = ["app": label, "run": run, "first_window_s": (windowAt * 1000).rounded() / 1000,
                               "footprint_mib": (memory / 1_048_576 * 10).rounded() / 10,
                               "idle_cpu_percent": (cpuSeconds / 10 * 1000).rounded() / 10, "processes": processes]
    let data = try JSONSerialization.data(withJSONObject: line, options: [.sortedKeys])
    print(String(decoding: data, as: UTF8.self))
    fflush(stdout)
    sleep(3)
}
