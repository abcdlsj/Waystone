import os

/// Unified logging categories; `log stream --predicate 'subsystem == "dev.waystone.app"'`.
enum Log {
    static let app = Logger(subsystem: "dev.waystone.app", category: "app")
    static let signal = Logger(subsystem: "dev.waystone.app", category: "signal")
    static let session = Logger(subsystem: "dev.waystone.app", category: "session")
    static let media = Logger(subsystem: "dev.waystone.app", category: "media")
}
