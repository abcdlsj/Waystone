import Foundation

/// Unattended-run hooks for development builds; compiled out of release.
///
/// A simulator cannot scan a QR code and a script cannot tap a phone, so:
///
/// * `WAYSTONE_PAIR_URI` (simulator: `SIMCTL_CHILD_WAYSTONE_PAIR_URI=…`), or a
///   file `Application Support/Waystone/pair.txt` copied in with `devicectl`,
///   pairs on launch when no server is enrolled yet;
/// * `WAYSTONE_AUTO_CONNECT=1`, or a second line `connect` in that file, keeps
///   starting a stream with the first online host.
enum DevHooks {
    @MainActor
    static func run(_ model: AppModel) {
        #if DEBUG
        let environment = ProcessInfo.processInfo.environment
        let inbox = Inbox.take()
        if let uri = environment["WAYSTONE_PAIR_URI"] ?? inbox?.uri, model.pools.isEmpty {
            Log.app.info("pairing from an injected link")
            model.pair(with: uri)
        }
        guard environment["WAYSTONE_AUTO_CONNECT"] != nil || inbox?.autoConnect == true else { return }
        Task { @MainActor in
            while !Task.isCancelled {
                model.autoConnect()
                try? await Task.sleep(for: .seconds(2))
            }
        }
        #endif
    }

    #if DEBUG
    private struct Inbox {
        let uri: String
        let autoConnect: Bool

        /// Reads and deletes the pairing file, if present.
        static func take() -> Inbox? {
            let file = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
                .appendingPathComponent("Waystone/pair.txt")
            guard let text = try? String(contentsOf: file, encoding: .utf8) else { return nil }
            try? FileManager.default.removeItem(at: file)
            let lines = text.split(whereSeparator: \.isNewline).map { $0.trimmingCharacters(in: .whitespaces) }
            guard let uri = lines.first else { return nil }
            return Inbox(uri: uri, autoConnect: lines.dropFirst().contains("connect"))
        }
    }
    #endif
}
