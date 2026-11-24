import Foundation

/// The persisted list of enrolled servers.
///
/// One small JSON file in Application Support: the data is tiny, has no
/// relations and is easy to inspect while developing.
@MainActor
final class InstanceStore: ObservableObject {
    @Published private(set) var instances: [Instance] = []

    private let fileURL: URL

    init(fileURL: URL = InstanceStore.defaultURL) {
        self.fileURL = fileURL
        if let data = try? Data(contentsOf: fileURL),
            let decoded = try? JSONDecoder().decode([Instance].self, from: data) {
            instances = decoded
        }
    }

    /// Adds a server, or refreshes its details when it is already known.
    func upsert(_ instance: Instance) {
        if let index = instances.firstIndex(where: { $0.id == instance.id }) {
            var updated = instance
            updated.label = instances[index].label
            instances[index] = updated
        } else {
            instances.append(instance)
        }
        save()
    }

    func remove(id: Instance.ID) {
        instances.removeAll { $0.id == id }
        save()
    }

    private func save() {
        guard let data = try? JSONEncoder().encode(instances) else { return }
        try? FileManager.default.createDirectory(
            at: fileURL.deletingLastPathComponent(), withIntermediateDirectories: true)
        try? data.write(to: fileURL, options: [.atomic, .completeFileProtectionUntilFirstUserAuthentication])
    }

    nonisolated static var defaultURL: URL {
        FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("Waystone/instances.json")
    }
}
